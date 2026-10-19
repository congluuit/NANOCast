//! One cast: capture (portal/PipeWire or X11) -> NV12 -> H.264 (FFmpeg or OpenH264) ->
//! paced UDP to one receiver, plus the control channel and, while switched on, system
//! sound (`audio::AudioSender`). Same API and behaviour as `nanocast_win::session`: runs on
//! its own thread; the caller (GUI) only starts it, reads `stats()`, toggles audio / pause /
//! blank and stops it. Resources are released in reverse order: capture stops first
//! (closing the portal session), then audio, the network thread, the encoder and finally
//! the control channel (`stop` is sent).
//!
//! Adaptive bitrate (`nanocast_core::adapt`): every receiver `stats` report is combined with
//! what was sent in that second; a new target bitrate is applied on the fly when the
//! encoder accepts it (`Encoder::set_bitrate`: libx264, NVENC), otherwise and for frame-rate
//! changes the encoder is reopened (its first frame is an IDR).
//!
//! HEVC is not implemented on Linux yet: H.264 is sent and `CastStats::codec_fallback`
//! says why (as on macOS).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use log::{error, info, warn};
use nanocast_core::adapt::{self, AdaptReason, Adaptive};
use nanocast_core::control::{Message, ERR_BAD_PIN};
use nanocast_core::protocol::{PacedSender, VideoCodec, AUDIO_PORT, MAX_PAYLOAD};

use crate::audio::{AudioCounters, AudioSender};
use crate::capture::{self, CaptureCounters, CaptureFlags, CapturedFrame, Source};
use crate::control_link::{ControlLink, ReceiverReport};
use crate::convert::fit_size;
use crate::encoder::{self, EncodedFrame, Encoder, EncoderConfig, Kind};
use crate::sources;

/// Largest picture sent, whatever the monitor (receivers are 1080p TVs).
pub const MAX_CANVAS: (u32, u32) = (1920, 1080);

#[derive(Clone, Debug)]
pub struct CastConfig {
    /// UDP video address of the receiver.
    pub video_addr: SocketAddr,
    /// TCP control port on the same host.
    pub ctrl_port: u16,
    pub source: Source,
    pub fps: u32,
    /// Starting bitrate, also the ceiling of the adaptive bitrate.
    pub bitrate_mbps: u32,
    /// Encoder name to try first (`h264_vaapi`, `h264_nvenc`, `h264_v4l2m2m`, `libx264`,
    /// `openh264`, see `mf_encoder::hw_encoder_names`); `None` = hardware first.
    pub encoder: Option<String>,
    /// Wanted codec. HEVC is not implemented on Linux yet: H.264 is sent and
    /// `CastStats::codec_fallback` says why.
    pub codec: VideoCodec,
    /// Lower / raise bitrate and fps from the receiver's reports (needs the control channel).
    pub adaptive: bool,
    /// The receiver's 4-digit code (`hello.pin`), when it asks for one.
    pub pin: Option<String>,
    /// Packet pacing ceiling (PLAN.md section 4: default 80 Mbps).
    pub pace_mbps: u64,
    /// Shown on the TV (`hello.name`).
    pub sender_name: String,
    /// Send system sound from the start (can be changed later with `set_audio`).
    pub audio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// `stop()` was called.
    Stopped,
    /// The captured window was closed, the monitor went away, or sharing was stopped from
    /// the desktop (portal indicator).
    SourceClosed,
    /// The receiver wants a 4-digit code and `CastConfig::pin` was missing or wrong.
    PinRejected,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Starting,
    Casting,
    Ended(EndReason),
    Failed(String),
}

/// Why HEVC was asked for but H.264 is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecFallback {
    /// `caps.codecs` has no "hevc" (or there is no control channel to ask).
    ReceiverH264Only,
    /// No HEVC encoder (on Linux: not implemented yet).
    NoEncoder,
}

#[derive(Clone, Debug, Default)]
pub struct CastStats {
    pub phase: Phase,
    pub encoder: String,
    /// Codec actually sent.
    pub codec: VideoCodec,
    pub codec_fallback: Option<CodecFallback>,
    /// Encoded picture size.
    pub size: (u32, u32),
    /// Averages over the last ~0.5 s.
    pub fps: f64,
    pub mbps: f64,
    pub encode_ms: f64,
    /// RGB -> NV12 (+ scaling) on the CPU.
    pub convert_ms: f64,
    /// Capture -> encoded frame ready to send.
    pub latency_ms: f64,
    pub frames_sent: u64,
    pub keyframes: u64,
    /// Frames skipped because the encoder or the network was busy.
    pub frames_dropped: u64,
    pub keyframe_requests: usize,
    /// Current encoder target (follows the adaptive bitrate).
    pub target_mbps: u32,
    pub target_fps: u32,
    /// Why the target is what it is (`AdaptReason::Start` = configured values).
    pub adapt_reason: AdaptReason,
    /// Adaptive bitrate running (asked for and a control channel is there).
    pub adaptive: bool,
    /// `pause {on}` / `blank {on}` in effect (capture and encoder idle).
    pub paused: bool,
    pub blank: bool,
    /// `None` when the receiver has no control channel (UDP only).
    pub receiver: Option<ReceiverReport>,
    /// Audio capture running (follows `set_audio`, false if it failed).
    pub audio_on: bool,
    /// Audio stream on the wire (Opus + headers), ~0.5 s average; ~4 kbps while silent.
    pub audio_kbps: f64,
    pub audio_device: Option<String>,
    pub audio_error: Option<String>,
}

/// Switches shared between the GUI and the cast thread.
#[derive(Default)]
struct Controls {
    stop: AtomicBool,
    audio: AtomicBool,
    pause: AtomicBool,
    blank: AtomicBool,
}

pub struct CastSession {
    controls: Arc<Controls>,
    stats: Arc<Mutex<CastStats>>,
    thread: Option<JoinHandle<()>>,
}

impl CastSession {
    /// Starts casting in the background. Failures show up as `Phase::Failed` in `stats()`.
    /// With the portal back end the system's screen-sharing dialog appears first.
    pub fn start(cfg: CastConfig) -> Self {
        let controls = Arc::new(Controls { audio: AtomicBool::new(cfg.audio), ..Default::default() });
        let stats = Arc::new(Mutex::new(CastStats::default()));
        let thread = {
            let (controls, stats) = (controls.clone(), stats.clone());
            std::thread::Builder::new()
                .name("cast".into())
                .spawn(move || {
                    let phase = match run(&cfg, &controls, &stats) {
                        Ok(reason) => {
                            info!("cast ended: {reason:?}");
                            Phase::Ended(reason)
                        }
                        Err(e) => {
                            error!("cast failed: {e:#}");
                            Phase::Failed(format!("{e:#}"))
                        }
                    };
                    stats.lock().unwrap().phase = phase;
                })
                .expect("spawn cast thread")
        };
        Self { controls, stats, thread: Some(thread) }
    }

    /// Switches system sound on or off; takes effect within one video frame and never
    /// blocks (the capture is opened and closed on its own thread).
    pub fn set_audio(&self, on: bool) {
        self.controls.audio.store(on, Ordering::Relaxed);
    }

    /// Pause: the TV keeps the last picture, capture / encoding / sound stop until resumed.
    pub fn set_paused(&self, on: bool) {
        self.controls.pause.store(on, Ordering::Relaxed);
    }

    /// Blank: the TV shows black; like pause, nothing is captured meanwhile.
    pub fn set_blank(&self, on: bool) {
        self.controls.blank.store(on, Ordering::Relaxed);
    }

    pub fn stats(&self) -> CastStats {
        self.stats.lock().unwrap().clone()
    }

    /// Still running (starting or casting).
    pub fn is_active(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stops and waits (up to 3 s) until capture, encoder and control channel are released.
    pub fn stop(&mut self) {
        self.controls.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else { return };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            warn!("cast thread did not stop within 3 s; leaving it behind");
        }
    }
}

impl Drop for CastSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stops the capture when dropped.
struct CaptureGuard(Option<capture::Handle>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            if let Err(e) = h.stop() {
                warn!("capture stop: {e:?}");
            }
        }
    }
}

/// What the network thread has put on the wire.
#[derive(Default)]
struct WireCounters {
    bytes: AtomicU64,
    packets: AtomicU64,
}

/// Network thread: encoded frames go through a small queue so packet pacing never
/// stalls the encoder loop. Joined when dropped.
struct NetSender {
    tx: Option<SyncSender<Vec<u8>>>,
    thread: Option<JoinHandle<()>>,
    dropped: u64,
}

impl NetSender {
    fn start(sock: UdpSocket, pace_mbps: u64, wire: Arc<WireCounters>) -> Result<Self> {
        let mut sender = PacedSender::new(sock, pace_mbps * 1_000_000, MAX_PAYLOAD);
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        let thread = std::thread::Builder::new().name("udp-send".into()).spawn(move || {
            let mut warned = false;
            while let Ok(au) = rx.recv() {
                match sender.send_frame(&au) {
                    Ok(Some(st)) => {
                        wire.bytes.fetch_add(st.bytes as u64, Ordering::Relaxed);
                        wire.packets.fetch_add(st.packets as u64, Ordering::Relaxed);
                    }
                    Ok(None) => warn!("frame of {} B too large to packetize; lower the bitrate", au.len()),
                    Err(e) if !warned => {
                        warn!("send error: {e}");
                        warned = true;
                    }
                    Err(_) => {}
                }
            }
        })?;
        Ok(Self { tx: Some(tx), thread: Some(thread), dropped: 0 })
    }

    fn push(&mut self, au: Vec<u8>) {
        if let Some(tx) = &self.tx {
            if tx.try_send(au).is_err() {
                self.dropped += 1;
            }
        }
    }
}

impl Drop for NetSender {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Collects encoder output: forwards to the network and accumulates stats.
struct Sink {
    net: NetSender,
    total: u64,
    keyframes: u64,
    // Window since the last stats publish.
    frames: u64,
    bytes: u64,
    encode_us: u64,
    latency_us: u64,
}

impl Sink {
    fn push(&mut self, f: EncodedFrame) {
        let now = Instant::now();
        self.total += 1;
        self.frames += 1;
        self.bytes += f.data.len() as u64;
        self.keyframes += f.keyframe as u64;
        self.encode_us += f.encode_latency_us;
        self.latency_us += now.duration_since(f.capture_instant).as_micros() as u64;
        self.net.push(f.data);
    }
}

/// The audio half of a cast: follows the wanted on/off state, tells the receiver.
struct AudioPart {
    addr: SocketAddr,
    sender: Option<AudioSender>,
    counters: Arc<AudioCounters>,
    last_bytes: u64,
}

impl AudioPart {
    /// Starts or stops audio when `want` changed; returns the new state if it did.
    fn follow(&mut self, want: bool) -> Option<bool> {
        if want == self.sender.is_some() {
            return None;
        }
        if want {
            info!("audio on -> {}", self.addr);
            self.sender = Some(AudioSender::start(self.addr, self.counters.clone()));
        } else if let Some(s) = self.sender.take() {
            info!("audio off");
            s.stop_detached();
        }
        Some(want)
    }
}

/// Adaptive bitrate state: the controller plus what the previous report saw.
struct Adapter {
    ctl: Adaptive,
    last_stats: u64,
    last_packets: u64,
    last_frames: u64,
    last_keyframe_requests: usize,
    last_at: Instant,
}

impl Adapter {
    /// Feeds a new receiver report (if one arrived); returns a new target to apply.
    fn poll(&mut self, link: &ControlLink, wire: &WireCounters, frames_sent: u64) -> Option<adapt::Target> {
        let (count, tv) = {
            let r = link.report.lock().unwrap();
            (r.stats_count, r.stats)
        };
        if count == self.last_stats {
            return None;
        }
        let tv = tv?;
        let now = Instant::now();
        let packets = wire.packets.load(Ordering::Relaxed);
        let requests = link.keyframe_requests.load(Ordering::Relaxed);
        let secs = now.duration_since(self.last_at).as_secs_f64().max(0.2);
        let report = adapt::Report {
            lost: tv.lost,
            sent_packets: packets - self.last_packets,
            sent_fps: (frames_sent - self.last_frames) as f64 / secs,
            keyframe_requests: (requests - self.last_keyframe_requests) as u32,
            tv_fps: tv.fps,
            decode_ms: tv.decode_ms,
        };
        (self.last_stats, self.last_packets, self.last_frames, self.last_keyframe_requests, self.last_at) =
            (count, packets, frames_sent, requests, now);
        let target = self.ctl.on_report(report, now)?;
        info!("adaptive bitrate: {} Mbps @ {} fps ({:?}; report {report:?})", target.mbps, target.fps, self.ctl.reason());
        Some(target)
    }
}

/// Encoder size and rate for `kind`: the source fitted into the receiver maximum and the
/// encoder's own limits (software encoders: 720p30, Raspberry Pi 4: 1080p30).
fn plan(kind: Kind, source: &Source, src_size: (u32, u32), max: (u32, u32), fps: u32) -> ((u32, u32), u32) {
    let lim = kind.limits();
    let max = (max.0.min(lim.max_size.0), max.1.min(lim.max_size.1));
    let canvas = match source {
        Source::Monitor(_) => fit_size(src_size.0.max(2), src_size.1.max(2), max.0, max.1),
        // A window is scaled into a full TV frame so resizing it never needs a new stream.
        Source::Window(_) => (max.0 & !1, max.1 & !1),
    };
    (canvas, fps.clamp(1, lim.max_fps))
}

fn run(cfg: &CastConfig, controls: &Controls, stats: &Mutex<CastStats>) -> Result<EndReason> {
    let stop = &controls.stop;
    let sock = UdpSocket::bind("0.0.0.0:0").context("UDP socket")?;
    sock.connect(cfg.video_addr).with_context(|| format!("UDP to {}", cfg.video_addr))?;

    // Control channel is best effort: without it the UDP stream still plays, it just
    // cannot ask for a keyframe after a loss.
    let want_keyframe = Arc::new(AtomicBool::new(false));
    let ctrl_addr = SocketAddr::new(cfg.video_addr.ip(), cfg.ctrl_port);
    let mut link = match ControlLink::open(ctrl_addr, &cfg.sender_name, cfg.pin.as_deref(), want_keyframe.clone()) {
        Ok(link) => {
            info!("control channel connected to {ctrl_addr}");
            Some(link)
        }
        Err(e) => {
            warn!("control channel to {ctrl_addr} unavailable ({e}); streaming UDP only");
            None
        }
    };

    // Open the source first: with the portal this is the system dialog (it also tells the
    // picture size). `caps` (or a PIN refusal) arrive meanwhile; a short wait covers X11.
    let prepared = match capture::prepare(&cfg.source, stop) {
        Ok(p) => p,
        Err(_) if stop.load(Ordering::Relaxed) => return Ok(EndReason::Stopped),
        Err(e) => return Err(e).context("capture"),
    };
    if let Some(l) = &link {
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            {
                let r = l.report.lock().unwrap();
                if r.codecs.is_some() || r.error.is_some() || !r.connected {
                    break;
                }
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(EndReason::Stopped);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let report = link.as_ref().map(|l| l.report.lock().unwrap().clone()).unwrap_or_default();
    if report.error.as_deref() == Some(ERR_BAD_PIN) {
        return Ok(EndReason::PinRejected);
    }
    let max = report.max_size.map(|(w, h)| (w.min(MAX_CANVAS.0), h.min(MAX_CANVAS.1))).unwrap_or(MAX_CANVAS);
    let codec_fallback = match cfg.codec {
        VideoCodec::H264 => None,
        VideoCodec::Hevc if VideoCodec::negotiate(VideoCodec::Hevc, report.codecs.as_deref()) == VideoCodec::H264 => {
            Some(CodecFallback::ReceiverH264Only)
        }
        VideoCodec::Hevc => {
            warn!("HEVC is not implemented on Linux yet; sending H.264");
            Some(CodecFallback::NoEncoder)
        }
    };
    let src_size = prepared.size();

    // With a control channel keyframes come on request; without one the TV can only
    // recover (or join late) at the next periodic keyframe, so keep those frequent.
    let has_link = link.is_some();
    let gop = |fps: u32| if has_link { fps * 10 } else { fps * 2 };

    // The first encoder that opens decides the canvas and the frame-rate ceiling
    // (software encoders run at 720p30). The canvas stays fixed for the whole cast.
    let mut bad_encoders: Vec<String> = Vec::new();
    let kinds = encoder::candidates(cfg.encoder.as_deref(), &bad_encoders);
    if kinds.is_empty() {
        bail!("no H.264 encoder available: install FFmpeg (libavcodec) with VA-API or x264, or build with the openh264 feature");
    }
    let mut opened = None;
    for &kind in &kinds {
        let (canvas, fps) = plan(kind, &cfg.source, src_size, max, cfg.fps);
        let enc_cfg =
            EncoderConfig { width: canvas.0, height: canvas.1, fps, bitrate_bps: cfg.bitrate_mbps.saturating_mul(1_000_000), gop_frames: gop(fps) };
        match Encoder::open(kind, &enc_cfg) {
            Ok(e) => {
                opened = Some((e, enc_cfg));
                break;
            }
            Err(e) => {
                warn!("encoder {} unavailable: {e:#}", kind.name());
                bad_encoders.push(kind.name().to_string());
            }
        }
    }
    let Some((mut encoder, mut enc_cfg)) = opened else {
        bail!("no H.264 encoder could be opened (tried {})", bad_encoders.join(", "));
    };
    let canvas = (enc_cfg.width, enc_cfg.height);
    let max_fps = enc_cfg.fps;

    let mut adapter = (cfg.adaptive && link.is_some()).then(|| Adapter {
        ctl: Adaptive::new(cfg.bitrate_mbps, max_fps, Instant::now()),
        last_stats: 0,
        last_packets: 0,
        last_frames: 0,
        last_keyframe_requests: 0,
        last_at: Instant::now(),
    });
    let mut target = adapt::Target { mbps: cfg.bitrate_mbps, fps: max_fps };

    let start_msg =
        Message::Start { w: canvas.0, h: canvas.1, fps: max_fps, codec: "h264".into(), audio: controls.audio.load(Ordering::Relaxed) };
    if let Some(l) = link.as_mut() {
        l.send(&start_msg);
    }
    {
        let mut s = stats.lock().unwrap();
        s.encoder = encoder.name.clone();
        s.codec = VideoCodec::H264;
        s.codec_fallback = codec_fallback;
        s.size = canvas;
        (s.target_mbps, s.target_fps, s.adaptive) = (target.mbps, target.fps, adapter.is_some());
    }

    let wire = Arc::new(WireCounters::default());
    let mut sink = Sink {
        net: NetSender::start(sock, cfg.pace_mbps, wire.clone())?,
        total: 0,
        keyframes: 0,
        frames: 0,
        bytes: 0,
        encode_us: 0,
        latency_us: 0,
    };

    let (tx, rx): (SyncSender<CapturedFrame>, Receiver<CapturedFrame>) = sync_channel(1);
    let counters = Arc::new(CaptureCounters::default());
    let interval_us = Arc::new(AtomicU64::new(1_000_000 / target.fps.max(1) as u64));
    let capture_paused = Arc::new(AtomicBool::new(false));
    let backend = prepared.backend();
    let capture = CaptureGuard(Some(prepared.start(CaptureFlags {
        tx,
        interval_us: interval_us.clone(),
        paused: capture_paused.clone(),
        canvas,
        counters: counters.clone(),
    })?));
    let mut audio = AudioPart {
        addr: SocketAddr::new(cfg.video_addr.ip(), AUDIO_PORT),
        sender: None,
        counters: Arc::new(AudioCounters::default()),
        last_bytes: 0,
    };
    // Already announced in `start`; no separate `audio` message for the initial state.
    audio.follow(controls.audio.load(Ordering::Relaxed));
    stats.lock().unwrap().phase = Phase::Casting;
    info!(
        "casting {:?} ({backend:?}) -> {} as h264 {}x{} @ {} fps, {} Mbps, {}{}",
        cfg.source,
        cfg.video_addr,
        canvas.0,
        canvas.1,
        target.fps,
        target.mbps,
        encoder.name,
        if adapter.is_some() { " (adaptive)" } else { "" }
    );

    let t0 = Instant::now();
    let mut last_publish = Instant::now();
    let mut convert_us = 0u64;
    let mut converted = 0u64;
    let mut last_alive_check = Instant::now();
    let (mut paused, mut blank) = (false, false);

    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break EndReason::Stopped;
        }
        if counters.closed.load(Ordering::Relaxed) || capture.0.as_ref().is_some_and(|h| h.is_finished()) {
            break EndReason::SourceClosed;
        }
        if link.as_ref().is_some_and(|l| l.report.lock().unwrap().error.as_deref() == Some(ERR_BAD_PIN)) {
            break EndReason::PinRejected;
        }

        // Pause / blank: tell the TV, idle the capture, mute; IDR when the picture resumes.
        let (want_pause, want_blank) = (controls.pause.load(Ordering::Relaxed), controls.blank.load(Ordering::Relaxed));
        if want_pause != paused || want_blank != blank {
            if let Some(l) = link.as_mut() {
                if want_pause != paused {
                    l.send(&Message::Pause { on: want_pause });
                }
                if want_blank != blank {
                    l.send(&Message::Blank { on: want_blank });
                }
            }
            let was_held = paused || blank;
            (paused, blank) = (want_pause, want_blank);
            capture_paused.store(paused || blank, Ordering::Relaxed);
            if was_held && !(paused || blank) {
                encoder.request_keyframe();
            }
            info!("pause {paused}, blank {blank}");
        }
        let held = paused || blank;
        if let Some(on) = audio.follow(controls.audio.load(Ordering::Relaxed) && !held) {
            if let Some(l) = link.as_mut() {
                l.send(&Message::Audio { on });
            }
        }
        if let Source::Window(id) = cfg.source {
            if last_alive_check.elapsed() >= Duration::from_secs(1) {
                last_alive_check = Instant::now();
                if !sources::window_alive(id) {
                    break EndReason::SourceClosed;
                }
            }
        }

        // Adaptive bitrate: at most one change per receiver report.
        if let (Some(a), Some(l)) = (adapter.as_mut(), link.as_ref()) {
            if let Some(next) = a.poll(l, &wire, sink.total) {
                let next_fps = next.fps.min(max_fps);
                enc_cfg.bitrate_bps = next.mbps.saturating_mul(1_000_000);
                interval_us.store(1_000_000 / next_fps.max(1) as u64, Ordering::Relaxed);
                let reopen = next_fps != enc_cfg.fps || !encoder.set_bitrate(enc_cfg.bitrate_bps);
                if reopen {
                    // A new encoder starts with an IDR, so the TV resyncs by itself.
                    enc_cfg.fps = next_fps;
                    enc_cfg.gop_frames = gop(next_fps);
                    let kind = encoder.kind;
                    drop(encoder); // free the hardware session before opening the next one
                    let mut order = vec![kind];
                    order.extend(encoder::candidates(None, &bad_encoders).into_iter().filter(|k| *k != kind));
                    encoder = Encoder::open_first_working(&order, &enc_cfg)?;
                    stats.lock().unwrap().encoder = encoder.name.clone();
                }
                target = next;
                let mut s = stats.lock().unwrap();
                (s.target_mbps, s.target_fps, s.adapt_reason) = (target.mbps, next_fps, a.ctl.reason());
            }
        }

        let step = match rx.recv_timeout(Duration::from_millis(5)) {
            // Frames captured just before a pause are not sent.
            Ok(_) if held => Ok(()),
            Ok(frame) => {
                convert_us += frame.convert_us;
                converted += 1;
                // keyframe_request from the receiver (set by the control reader thread).
                if want_keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                let pts_us = frame.captured.duration_since(t0).as_micros() as i64;
                encoder.submit(&frame.nv12, pts_us, frame.captured).and_then(|()| encoder.poll_output(|f| sink.push(f)))
            }
            Err(RecvTimeoutError::Timeout) => encoder.poll_output(|f| sink.push(f)),
            Err(RecvTimeoutError::Disconnected) => break EndReason::SourceClosed,
        };
        if let Err(e) = step {
            // Some encoders open fine but fail on the first frame (wrong VA driver, busy
            // V4L2 device). Before the first frame went out, move on to the next one at the
            // same size; after it, an error is fatal.
            if sink.total > 0 {
                return Err(e).context(format!("encoder {}", encoder.name));
            }
            warn!("encoder {} failed before its first frame ({e:#}); trying the next one", encoder.name);
            bad_encoders.push(encoder.name.clone());
            drop(encoder);
            let next = encoder::candidates(None, &bad_encoders);
            encoder = Encoder::open_first_working(&next, &enc_cfg).context("no other encoder works")?;
            encoder.request_keyframe();
            stats.lock().unwrap().encoder = encoder.name.clone();
            continue;
        }

        let elapsed = last_publish.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let secs = elapsed.as_secs_f64();
            let per_frame = |us: u64, n: u64| if n == 0 { 0.0 } else { us as f64 / n as f64 / 1000.0 };
            let mut s = stats.lock().unwrap();
            s.fps = sink.frames as f64 / secs;
            s.mbps = sink.bytes as f64 * 8.0 / secs / 1e6;
            s.encode_ms = per_frame(sink.encode_us, sink.frames);
            s.latency_ms = per_frame(sink.latency_us, sink.frames);
            s.convert_ms = per_frame(convert_us, converted);
            s.frames_sent = sink.total;
            s.keyframes = sink.keyframes;
            s.frames_dropped = counters.dropped.load(Ordering::Relaxed) as u64 + sink.net.dropped + encoder.dropped();
            (s.paused, s.blank) = (paused, blank);
            let audio_bytes = audio.counters.bytes.load(Ordering::Relaxed);
            s.audio_kbps = (audio_bytes - audio.last_bytes) as f64 * 8.0 / secs / 1e3;
            audio.last_bytes = audio_bytes;
            s.audio_on = audio.sender.is_some() && audio.counters.error.lock().unwrap().is_none();
            s.audio_device = audio.counters.device.lock().unwrap().clone();
            s.audio_error = audio.counters.error.lock().unwrap().clone();
            if let Some(l) = &link {
                s.keyframe_requests = l.keyframe_requests.load(Ordering::Relaxed);
                s.receiver = Some(l.report.lock().unwrap().clone());
            }
            drop(s);
            (sink.frames, sink.bytes, sink.encode_us, sink.latency_us, convert_us, converted) = (0, 0, 0, 0, 0, 0);
            last_publish = Instant::now();
        }
    };

    // Release in order: capture (and the portal session) and audio, network, encoder,
    // then `stop` to the receiver.
    drop(capture);
    drop(audio.sender.take());
    drop(rx);
    drop(sink);
    drop(encoder);
    if let Some(l) = link.take() {
        l.close();
    }
    Ok(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_caps_software_encoders() {
        let src = (2560, 1440);
        assert_eq!(plan(Kind::Vaapi, &Source::Monitor(1), src, MAX_CANVAS, 60), ((1920, 1080), 60));
        assert_eq!(plan(Kind::X264, &Source::Monitor(1), src, MAX_CANVAS, 60), ((1280, 720), 30));
        assert_eq!(plan(Kind::V4l2m2m, &Source::Window(5), src, MAX_CANVAS, 60), ((1920, 1080), 30));
        assert_eq!(plan(Kind::OpenH264, &Source::Monitor(1), (1366, 768), MAX_CANVAS, 30), ((1280, 718), 30));
    }
}
