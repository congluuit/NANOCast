//! One cast: ScreenCaptureKit -> VideoToolbox H.264 -> paced UDP to one receiver, plus the
//! control channel and, while switched on, system sound (`audio::AudioSender`). Same API
//! and behaviour as `nanocast_win::session`: runs on its own thread; the caller (GUI) only
//! starts it, reads `stats()` and stops it. Resources are released in reverse order:
//! capture stops first, then audio, the network thread, the encoder and finally the
//! control channel (`stop` is sent).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use log::{error, info, warn};
use nanocast_core::control::Message;
use nanocast_core::protocol::{PacedSender, AUDIO_PORT, MAX_PAYLOAD};

use crate::audio::{AudioCounters, AudioSender, AudioTap};
use crate::capture::{self, CaptureCounters, CaptureFlags, CapturedFrame, Source};
use crate::control_link::{ControlLink, ReceiverReport};
use crate::convert::fit_size;
use crate::sources;
use crate::vt_encoder::{EncodedFrame, EncoderConfig, VtEncoder};

/// Largest picture sent, whatever the display (receivers are 1080p TVs).
pub const MAX_CANVAS: (u32, u32) = (1920, 1080);

#[derive(Clone, Debug)]
pub struct CastConfig {
    /// UDP video address of the receiver.
    pub video_addr: SocketAddr,
    /// TCP control port on the same host.
    pub ctrl_port: u16,
    pub source: Source,
    pub fps: u32,
    pub bitrate_mbps: u32,
    /// Kept for API parity with Windows; a Mac has one H.264 encoder (VideoToolbox).
    pub encoder: Option<String>,
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
    /// The captured window was closed (or the display went away).
    SourceClosed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Starting,
    Casting,
    Ended(EndReason),
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct CastStats {
    pub phase: Phase,
    pub encoder: String,
    /// Encoded picture size.
    pub size: (u32, u32),
    /// Averages over the last ~0.5 s.
    pub fps: f64,
    pub mbps: f64,
    pub encode_ms: f64,
    /// Always 0 on macOS (ScreenCaptureKit scales and converts on the GPU).
    pub convert_ms: f64,
    /// Capture -> encoded frame ready to send.
    pub latency_ms: f64,
    pub frames_sent: u64,
    pub keyframes: u64,
    /// Frames skipped because the encoder or the network was busy (or dropped by rate control).
    pub frames_dropped: u64,
    pub keyframe_requests: usize,
    /// `None` when the receiver has no control channel (UDP only).
    pub receiver: Option<ReceiverReport>,
    /// Audio capture running (follows `set_audio`, false if unavailable).
    pub audio_on: bool,
    /// Audio stream on the wire (Opus + headers), ~0.5 s average; ~4 kbps while silent.
    pub audio_kbps: f64,
    pub audio_device: Option<String>,
    pub audio_error: Option<String>,
}

pub struct CastSession {
    stop: Arc<AtomicBool>,
    audio: Arc<AtomicBool>,
    stats: Arc<Mutex<CastStats>>,
    thread: Option<JoinHandle<()>>,
}

impl CastSession {
    /// Starts casting in the background. Failures show up as `Phase::Failed` in `stats()`.
    pub fn start(cfg: CastConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let audio = Arc::new(AtomicBool::new(cfg.audio));
        let stats = Arc::new(Mutex::new(CastStats::default()));
        let thread = {
            let (stop, audio, stats) = (stop.clone(), audio.clone(), stats.clone());
            std::thread::Builder::new()
                .name("cast".into())
                .spawn(move || {
                    let phase = match run(&cfg, &stop, &audio, &stats) {
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
        Self { stop, audio, stats, thread: Some(thread) }
    }

    /// Switches system sound on or off; takes effect within one video frame and never blocks.
    pub fn set_audio(&self, on: bool) {
        self.audio.store(on, Ordering::Relaxed);
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
        self.stop.store(true, Ordering::Relaxed);
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

/// Network thread: encoded frames go through a small queue so packet pacing never
/// stalls the encoder loop. Joined when dropped.
struct NetSender {
    tx: Option<SyncSender<Vec<u8>>>,
    thread: Option<JoinHandle<()>>,
    dropped: u64,
}

impl NetSender {
    fn start(sock: UdpSocket, pace_mbps: u64, sent_bytes: Arc<AtomicU64>) -> Result<Self> {
        let mut sender = PacedSender::new(sock, pace_mbps * 1_000_000, MAX_PAYLOAD);
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        let thread = std::thread::Builder::new().name("udp-send".into()).spawn(move || {
            let mut warned = false;
            while let Ok(au) = rx.recv() {
                match sender.send_frame(&au) {
                    Ok(Some(st)) => {
                        sent_bytes.fetch_add(st.bytes as u64, Ordering::Relaxed);
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
    /// `None` on macOS 12 (no ScreenCaptureKit audio).
    tap: Option<Arc<AudioTap>>,
    sender: Option<AudioSender>,
    /// "on" while unavailable, so the state is reported once and not retried every frame.
    unavailable_on: bool,
    counters: Arc<AudioCounters>,
    last_bytes: u64,
}

impl AudioPart {
    fn is_on(&self) -> bool {
        self.sender.is_some() || self.unavailable_on
    }

    /// Starts or stops audio when `want` changed; returns the new state if it did.
    fn follow(&mut self, want: bool) -> Option<bool> {
        if want == self.is_on() {
            return None;
        }
        if want {
            match &self.tap {
                Some(tap) => {
                    info!("audio on -> {}", self.addr);
                    self.sender = Some(AudioSender::start(self.addr, tap.clone(), self.counters.clone()));
                }
                None => {
                    AudioSender::unavailable("system sound needs macOS 13 or later", &self.counters);
                    self.unavailable_on = true;
                }
            }
        } else {
            self.unavailable_on = false;
            if let Some(s) = self.sender.take() {
                info!("audio off");
                s.stop_detached();
            }
        }
        Some(want)
    }
}

fn run(cfg: &CastConfig, stop: &AtomicBool, audio_on: &AtomicBool, stats: &Mutex<CastStats>) -> Result<EndReason> {
    if !sources::has_screen_recording_permission() {
        // Shows the system prompt the first time; capture start below then fails with a
        // clear message until the user allows NANOCast and restarts it.
        sources::request_screen_recording_permission();
    }

    let sock = UdpSocket::bind("0.0.0.0:0").context("UDP socket")?;
    sock.connect(cfg.video_addr).with_context(|| format!("UDP to {}", cfg.video_addr))?;

    // Control channel is best effort: without it the UDP stream still plays, it just
    // cannot ask for a keyframe after a loss.
    let want_keyframe = Arc::new(AtomicBool::new(false));
    let ctrl_addr = SocketAddr::new(cfg.video_addr.ip(), cfg.ctrl_port);
    let mut link = match ControlLink::open(ctrl_addr, &cfg.sender_name, want_keyframe.clone()) {
        Ok(link) => {
            info!("control channel connected to {ctrl_addr}");
            Some(link)
        }
        Err(e) => {
            warn!("control channel to {ctrl_addr} unavailable ({e}); streaming UDP only");
            None
        }
    };
    // Give `caps` a moment so the picture size can respect the receiver's maximum.
    if let Some(l) = &link {
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until && l.report.lock().unwrap().max_size.is_none() {
            if stop.load(Ordering::Relaxed) {
                return Ok(EndReason::Stopped);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let max = link
        .as_ref()
        .and_then(|l| l.report.lock().unwrap().max_size)
        .map(|(w, h)| (w.min(MAX_CANVAS.0), h.min(MAX_CANVAS.1)))
        .unwrap_or(MAX_CANVAS);

    // Fixed encoder size: a display keeps its aspect (capped); a window is scaled into a
    // full TV frame so resizing it never needs a new stream.
    let canvas = match &cfg.source {
        Source::Monitor(_) => {
            let (w, h) = capture::source_size(&cfg.source)?;
            fit_size(w, h, max.0, max.1)
        }
        Source::Window(_) => (max.0 & !1, max.1 & !1),
    };

    let enc_cfg = EncoderConfig {
        width: canvas.0,
        height: canvas.1,
        fps: cfg.fps,
        bitrate_bps: cfg.bitrate_mbps * 1_000_000,
        // With a control channel keyframes come on request; without one the TV can only
        // recover (or join late) at the next periodic keyframe, so keep those frequent.
        gop_frames: if link.is_some() { cfg.fps * 10 } else { cfg.fps * 2 },
    };
    let mut encoder = VtEncoder::open(&enc_cfg)?;
    let start_msg =
        Message::Start { w: canvas.0, h: canvas.1, fps: cfg.fps, codec: "h264".into(), audio: audio_on.load(Ordering::Relaxed) };
    if let Some(l) = link.as_mut() {
        l.send(&start_msg);
    }
    {
        let mut s = stats.lock().unwrap();
        s.encoder = encoder.name.clone();
        s.size = canvas;
    }

    let sent_bytes = Arc::new(AtomicU64::new(0));
    let mut sink = Sink {
        net: NetSender::start(sock, cfg.pace_mbps, sent_bytes)?,
        total: 0,
        keyframes: 0,
        frames: 0,
        bytes: 0,
        encode_us: 0,
        latency_us: 0,
    };

    // System sound is tapped from the same SCStream (macOS 13+); the tap only forwards
    // while an AudioSender is connected, so `set_audio` needs no stream restart.
    let tap = sources::audio_supported().then(AudioTap::new);
    let (tx, rx): (SyncSender<CapturedFrame>, Receiver<CapturedFrame>) = sync_channel(1);
    let counters = Arc::new(CaptureCounters::default());
    let flags = CaptureFlags {
        tx,
        min_interval: Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64),
        canvas,
        counters: counters.clone(),
        audio: tap.clone(),
    };
    let capture = capture::start(&cfg.source, flags)?;
    let mut audio = AudioPart {
        addr: SocketAddr::new(cfg.video_addr.ip(), AUDIO_PORT),
        tap,
        sender: None,
        unavailable_on: false,
        counters: Arc::new(AudioCounters::default()),
        last_bytes: 0,
    };
    // Already announced in `start`; no separate `audio` message for the initial state.
    audio.follow(audio_on.load(Ordering::Relaxed));
    stats.lock().unwrap().phase = Phase::Casting;
    info!("casting {:?} -> {} as {}x{} @ {} fps, {} Mbps", cfg.source, cfg.video_addr, canvas.0, canvas.1, cfg.fps, cfg.bitrate_mbps);

    let t0 = Instant::now();
    let mut last_publish = Instant::now();
    let mut last_alive_check = Instant::now();
    let mut retried_normal = false;

    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break EndReason::Stopped;
        }
        if capture.is_finished() {
            break EndReason::SourceClosed;
        }
        if let Some(on) = audio.follow(audio_on.load(Ordering::Relaxed)) {
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

        let step = match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(frame) => {
                // keyframe_request from the receiver (set by the control reader thread).
                if want_keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                let pts_hns = frame.captured.duration_since(t0).as_nanos() as i64 / 100;
                encoder.submit(&frame.pixels, pts_hns, frame.captured).and_then(|()| encoder.poll_output(|f| sink.push(f)))
            }
            Err(RecvTimeoutError::Timeout) => encoder.poll_output(|f| sink.push(f)),
            Err(RecvTimeoutError::Disconnected) => break EndReason::SourceClosed,
        };
        if let Err(e) = step {
            // A low-latency session that fails before its first frame gets one retry with
            // the normal rate controller. After the first frame, an error is fatal.
            if sink.total > 0 || retried_normal || !encoder.low_latency {
                return Err(e).context(format!("encoder {}", encoder.name));
            }
            warn!("encoder {} failed before its first frame ({e:#}); retrying without low-latency rate control", encoder.name);
            retried_normal = true;
            encoder = VtEncoder::new(&enc_cfg, false)?;
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
            s.convert_ms = 0.0;
            s.frames_sent = sink.total;
            s.keyframes = sink.keyframes;
            s.frames_dropped = counters.dropped.load(Ordering::Relaxed) as u64 + sink.net.dropped + encoder.dropped();
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
            (sink.frames, sink.bytes, sink.encode_us, sink.latency_us) = (0, 0, 0, 0);
            last_publish = Instant::now();
        }
    };

    // Release in order: capture and audio, network, encoder, then `stop` to the receiver.
    if let Err(e) = capture.stop() {
        warn!("capture stop: {e:#}");
    }
    drop(audio.sender.take());
    drop(rx);
    drop(sink);
    drop(encoder);
    if let Some(l) = link.take() {
        l.close();
    }
    Ok(reason)
}
