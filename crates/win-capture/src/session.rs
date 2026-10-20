//! One cast: capture -> NV12 -> hardware H.264 / HEVC -> paced UDP to one receiver, plus the
//! control channel and, while switched on, system sound (`audio::AudioSender`). Runs on its
//! own thread; the caller (GUI) only starts it, reads `stats()`, toggles audio / pause /
//! blank and stops it. Every resource is released by RAII guards in reverse order:
//! capture stops first, then the network thread, the encoder, the control channel
//! (`stop` is sent), and finally Media Foundation.
//!
//! Adaptive bitrate (`nanocast_core::adapt`): every receiver `stats` report is combined with
//! what was sent in that second; a new target bitrate is applied on the fly when the MFT
//! accepts it (`MfEncoder::set_bitrate`), otherwise and for frame-rate changes the encoder is
//! reopened (its first frame is an IDR).
//!
//! Low CPU (PLAN.md Phase 1): BGRA -> NV12 runs on the GPU and the NV12 texture goes straight
//! into a D3D11-aware encoder (`gpu`, `MfEncoder::submit_texture`); otherwise, or when the
//! GPU path fails, the CPU converter is used. `CastConfig::convert` (or `NANOCAST_CONVERT=gpu|cpu`)
//! forces one. Unchanged frames are not encoded (`CastConfig::skip_static`, `NANOCAST_SKIP_STATIC=0`
//! turns that off); the last
//! frame is repeated after `REPEAT_AFTER` without a new one so the TV keeps its clock.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use log::{error, info, warn};
use nanocast_core::adapt::{self, AdaptReason, Adaptive};
use nanocast_core::control::{Message, ERR_BAD_PIN};
use nanocast_core::protocol::{PacedSender, VideoCodec, AUDIO_PORT, MAX_PAYLOAD};

use crate::audio::{AudioCounters, AudioSender};

use crate::capture::{self, CaptureCounters, CaptureFlags, CapturedFrame, Pixels, Source, MODE_CPU, MODE_GPU, MODE_WAIT};
use crate::control_link::{ControlLink, ReceiverReport};
use crate::convert::fit_size;
use crate::gpu::{device_manager, GpuFrame};
use crate::mf_encoder::{EncodedFrame, EncoderConfig, MfEncoder, MfRuntime};
use crate::sources;

/// Largest picture sent, whatever the monitor (receivers are 1080p TVs).
pub const MAX_CANVAS: (u32, u32) = (1920, 1080);
/// Longest gap between two encoded frames while the screen does not change.
const REPEAT_AFTER: Duration = Duration::from_secs(1);
/// GPU path health check: capture -> encoded above this for `GPU_SLOW_WINDOWS` stats windows
/// in a row (0.5 s each) switches to the CPU path. Normal is 12-16 ms; seen 40-370 ms when the
/// process had only 2 logical CPUs (docs/benchmark.md).
const GPU_SLOW_MS: f64 = 60.0;
const GPU_SLOW_WINDOWS: u32 = 3;

/// Where BGRA -> NV12 runs (`CastConfig::convert`, overridden by `NANOCAST_CONVERT`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConvertPath {
    /// GPU when the device, the video processor and the encoder allow it, else CPU.
    #[default]
    Auto,
    /// GPU or fail (for testing).
    Gpu,
    Cpu,
}

impl ConvertPath {
    /// `NANOCAST_CONVERT=auto|gpu|cpu`, `None` when unset or unknown.
    pub fn from_env() -> Option<Self> {
        match std::env::var("NANOCAST_CONVERT").ok()?.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "gpu" => Some(Self::Gpu),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

/// `NANOCAST_SKIP_STATIC=0|1` overrides `CastConfig::skip_static` (for testing).
fn skip_static_from_env() -> Option<bool> {
    std::env::var("NANOCAST_SKIP_STATIC").ok().map(|v| v != "0")
}
/// How long after an on-the-fly bitrate change the real rate is compared with the target.
const BITRATE_CHECK_AFTER: Duration = Duration::from_millis(1500);

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
    /// Encoder friendly name to try first; `None` = merit order.
    pub encoder: Option<String>,
    /// Wanted codec; HEVC falls back to H.264 when the receiver or the PC cannot do it.
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
    /// Where BGRA -> NV12 runs; `NANOCAST_CONVERT` overrides it.
    pub convert: ConvertPath,
    /// Do not encode frames while the screen does not change (one repeat per `REPEAT_AFTER`);
    /// `NANOCAST_SKIP_STATIC` overrides it.
    pub skip_static: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// `stop()` was called.
    Stopped,
    /// The captured window was closed (or the monitor went away).
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
    /// No hardware HEVC encoder on this PC could be started.
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
    /// CPU time per frame to get it to the encoder (GPU path: hash + copy + video processor
    /// submission; CPU path: read back + conversion).
    pub convert_ms: f64,
    /// BGRA -> NV12 on the GPU and textures into the encoder (zero-copy path).
    pub gpu_convert: bool,
    /// Frames not encoded because the screen did not change.
    pub frames_unchanged: u64,
    /// Frames re-encoded after `REPEAT_AFTER` without a change.
    pub frames_repeated: u64,
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
    /// Audio capture running (follows `set_audio`, false if the device failed).
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
    /// blocks (the audio device is opened and closed on its own thread).
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

/// Last frame given to the encoder, kept to repeat it while the screen does not change.
enum LastFrame {
    Cpu(Vec<u8>),
    /// Its pool texture stays reserved while kept.
    Gpu(GpuFrame),
}

/// Frame flow state between capture and encoder.
struct Pipe {
    /// GPU path available (capture converts on the GPU, `EncoderConfig::d3d` is its device).
    gpu: bool,
    /// Capture's `CaptureFlags::mode`.
    mode: Arc<AtomicU8>,
    /// Capture's `CaptureFlags::force_next`.
    force_next: Arc<AtomicBool>,
    last: Option<LastFrame>,
    /// GPU frames still read by the encoder (their textures stay reserved).
    encoding: Vec<GpuFrame>,
    last_input: Instant,
    repeated: u64,
}

impl Pipe {
    /// Tells the capture which kind of frame the current encoder takes.
    fn follow(&mut self, encoder: &MfEncoder) {
        let gpu = encoder.gpu_input && self.gpu;
        self.mode.store(if gpu { MODE_GPU } else { MODE_CPU }, Ordering::Relaxed);
        self.force_next.store(true, Ordering::Relaxed);
        self.encoding.clear();
    }

    /// Gives up the GPU path for the rest of the cast; the caller reopens the encoder
    /// (without the D3D manager) when it took textures.
    fn drop_gpu(&mut self, why: &str, enc_cfg: &mut EncoderConfig) {
        warn!("GPU conversion off ({why}); converting on the CPU");
        self.gpu = false;
        enc_cfg.d3d = None;
        if matches!(self.last, Some(LastFrame::Gpu(_))) {
            self.last = None;
        }
        self.encoding.clear();
        self.mode.store(MODE_CPU, Ordering::Relaxed);
        self.force_next.store(true, Ordering::Relaxed);
    }

    /// Frees the textures the encoder is done with.
    fn release_done(&mut self, enc: &MfEncoder) {
        self.encoding.retain(|f| enc.slot_busy(f.slot));
    }

    fn keep_last(&mut self, frame: LastFrame) {
        if let Some(LastFrame::Gpu(prev)) = self.last.replace(frame) {
            self.encoding.push(prev);
        }
        self.last_input = Instant::now();
    }

    /// Encodes one captured frame (or drops it when it does not fit the current encoder).
    /// Returns the frame's conversion time when it was encoded.
    fn encode(&mut self, enc: &mut MfEncoder, sink: &mut Sink, frame: CapturedFrame, t0: Instant) -> Result<Option<u64>> {
        let pts_hns = frame.captured.duration_since(t0).as_nanos() as i64 / 100;
        match frame.pixels {
            Pixels::Cpu(nv12) if !enc.gpu_input => {
                encode_one(enc, sink, &nv12, pts_hns, frame.captured)?;
                self.keep_last(LastFrame::Cpu(nv12));
            }
            Pixels::Gpu(f) if enc.gpu_input && self.gpu => {
                encode_texture(enc, sink, &f, pts_hns, frame.captured)?;
                self.keep_last(LastFrame::Gpu(f));
            }
            // Captured before the encoder changed kind.
            _ => {
                self.force_next.store(true, Ordering::Relaxed);
                return Ok(None);
            }
        }
        self.release_done(enc);
        Ok(Some(frame.convert_us))
    }

    /// Re-encodes the last frame when nothing was encoded for `REPEAT_AFTER`.
    fn repeat_if_due(&mut self, enc: &mut MfEncoder, sink: &mut Sink, t0: Instant) -> Result<()> {
        self.release_done(enc);
        if self.last_input.elapsed() < REPEAT_AFTER {
            return Ok(());
        }
        let now = Instant::now();
        let pts_hns = now.duration_since(t0).as_nanos() as i64 / 100;
        match &self.last {
            Some(LastFrame::Cpu(nv12)) if !enc.gpu_input => encode_one(enc, sink, nv12, pts_hns, now)?,
            Some(LastFrame::Gpu(f)) if enc.gpu_input && !enc.slot_busy(f.slot) => encode_texture(enc, sink, f, pts_hns, now)?,
            _ => return Ok(()),
        }
        self.last_input = now;
        self.repeated += 1;
        Ok(())
    }
}

/// Opens an encoder for `enc_cfg.codec`; HEVC that cannot be started falls back to H.264
/// (and changes `enc_cfg.codec`).
fn open_encoder(
    enc_cfg: &mut EncoderConfig,
    preferred: Option<&str>,
    skip: &[String],
    fallback: &mut Option<CodecFallback>,
) -> Result<MfEncoder> {
    match MfEncoder::open_first_working(enc_cfg, preferred, skip) {
        Ok(enc) => Ok(enc),
        Err(e) if enc_cfg.codec == VideoCodec::Hevc => {
            warn!("no usable HEVC encoder ({e:#}); sending H.264");
            enc_cfg.codec = VideoCodec::H264;
            *fallback = Some(CodecFallback::NoEncoder);
            MfEncoder::open_first_working(enc_cfg, preferred, skip)
        }
        Err(e) => Err(e),
    }
}

fn run(cfg: &CastConfig, controls: &Controls, stats: &Mutex<CastStats>) -> Result<EndReason> {
    let _rt = MfRuntime::init()?;
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
    // Give `caps` (or a PIN refusal) a moment so the picture size can respect the
    // receiver's maximum and the codec can follow its list.
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
    let mut fallback = None;
    let codec = VideoCodec::negotiate(cfg.codec, report.codecs.as_deref());
    if codec != cfg.codec {
        info!("receiver decodes {:?}: sending H.264 instead of {:?}", report.codecs, cfg.codec);
        fallback = Some(CodecFallback::ReceiverH264Only);
    }

    // Fixed encoder size: a monitor keeps its own size (capped); a window is scaled into
    // a full TV frame so resizing it never needs a new stream.
    let canvas = match &cfg.source {
        Source::Monitor(_) => {
            let (w, h) = capture::source_size(&cfg.source)?;
            fit_size(w, h, max.0, max.1)
        }
        Source::Window(_) => (max.0 & !1, max.1 & !1),
    };

    let mut adapter = (cfg.adaptive && link.is_some()).then(|| Adapter {
        ctl: Adaptive::new(cfg.bitrate_mbps, cfg.fps, Instant::now()),
        last_stats: 0,
        last_packets: 0,
        last_frames: 0,
        last_keyframe_requests: 0,
        last_at: Instant::now(),
    });
    let mut target = adapt::Target { mbps: cfg.bitrate_mbps, fps: cfg.fps };
    // With a control channel keyframes come on request; without one the TV can only
    // recover (or join late) at the next periodic keyframe, so keep those frequent.
    let has_link = link.is_some();
    let gop = |fps: u32| if has_link { fps * 10 } else { fps * 2 };

    // Capture starts first: with the GPU path the encoder works on the capture's device.
    let path = ConvertPath::from_env().unwrap_or(cfg.convert);
    let (tx, rx): (SyncSender<CapturedFrame>, Receiver<CapturedFrame>) = sync_channel(2);
    let (gpu_tx, gpu_rx) = sync_channel(1);
    let counters = Arc::new(CaptureCounters::default());
    let interval_us = Arc::new(AtomicU64::new(1_000_000 / target.fps.max(1) as u64));
    let capture_paused = Arc::new(AtomicBool::new(false));
    let mode = Arc::new(AtomicU8::new(MODE_WAIT));
    let force_next = Arc::new(AtomicBool::new(false));
    let flags = CaptureFlags {
        tx,
        interval_us: interval_us.clone(),
        paused: capture_paused.clone(),
        canvas,
        counters: counters.clone(),
        mode: mode.clone(),
        gpu_tx: (path != ConvertPath::Cpu).then_some(gpu_tx),
        skip_static: skip_static_from_env().unwrap_or(cfg.skip_static),
        force_next: force_next.clone(),
    };
    let capture = CaptureGuard(Some(capture::start(&cfg.source, flags)?));
    let d3d = if path == ConvertPath::Cpu {
        None
    } else {
        let device = gpu_rx.recv_timeout(Duration::from_secs(3)).ok().flatten();
        match device.map(|d| device_manager(&d)) {
            Some(Ok(m)) => Some(m),
            Some(Err(e)) => {
                warn!("GPU conversion unavailable ({e:#}); converting on the CPU");
                None
            }
            None => None,
        }
    };
    if path == ConvertPath::Gpu && d3d.is_none() {
        anyhow::bail!("GPU conversion unavailable (conversion forced to GPU)");
    }

    let mut enc_cfg = EncoderConfig {
        codec,
        width: canvas.0,
        height: canvas.1,
        fps: target.fps,
        bitrate_bps: target.mbps * 1_000_000,
        gop_frames: gop(target.fps),
        encoder_index: 0,
        d3d,
    };
    let mut bad_encoders = Vec::new();
    let mut encoder = open_encoder(&mut enc_cfg, cfg.encoder.as_deref(), &bad_encoders, &mut fallback)?;
    if path == ConvertPath::Gpu && !encoder.gpu_input {
        anyhow::bail!("encoder {} does not take D3D11 textures (conversion forced to GPU)", encoder.name);
    }
    let mut pipe = Pipe {
        gpu: enc_cfg.d3d.is_some(),
        mode,
        force_next,
        last: None,
        encoding: Vec::new(),
        last_input: Instant::now(),
        repeated: 0,
    };
    pipe.follow(&encoder);
    let start_msg = Message::Start {
        w: canvas.0,
        h: canvas.1,
        fps: cfg.fps,
        codec: enc_cfg.codec.as_str().into(),
        audio: controls.audio.load(Ordering::Relaxed),
    };
    if let Some(l) = link.as_mut() {
        l.send(&start_msg);
    }
    {
        let mut s = stats.lock().unwrap();
        s.encoder = encoder.name.clone();
        s.gpu_convert = encoder.gpu_input && pipe.gpu;
        s.codec = enc_cfg.codec;
        s.codec_fallback = fallback;
        s.size = canvas;
        s.target_mbps = target.mbps;
        s.target_fps = target.fps;
        s.adaptive = adapter.is_some();
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
        "casting {:?} -> {} as {} {}x{} @ {} fps, {} Mbps{}, NV12 on the {}",
        cfg.source,
        cfg.video_addr,
        enc_cfg.codec.as_str(),
        canvas.0,
        canvas.1,
        target.fps,
        target.mbps,
        if adapter.is_some() { " (adaptive)" } else { "" },
        if encoder.gpu_input && pipe.gpu { "GPU" } else { "CPU" }
    );

    let t0 = Instant::now();
    let mut last_publish = Instant::now();
    let mut convert_us = 0u64;
    let mut converted = 0u64;
    let mut last_alive_check = Instant::now();
    let (mut paused, mut blank) = (false, false);
    // On-the-fly bitrate change to verify: (target Mbps, when to look).
    let mut bitrate_check: Option<(u32, Instant)> = None;
    // Set once this encoder accepted a dynamic bitrate change but kept the old rate
    // (seen with Intel QSV): from then on every change reopens the encoder.
    let mut dynamic_ignored = false;
    let mut reopen_pending = false;
    let mut gpu_slow = 0u32;

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
        if let Source::Window(hwnd) = cfg.source {
            if last_alive_check.elapsed() >= Duration::from_secs(1) {
                last_alive_check = Instant::now();
                if !sources::window_alive(hwnd) {
                    break EndReason::SourceClosed;
                }
            }
        }

        // Adaptive bitrate: at most one change per receiver report. The bitrate is first
        // changed on the fly; `bitrate_check` then verifies that the MFT honoured it.
        let mut reopen = std::mem::take(&mut reopen_pending);
        if let (Some(a), Some(l)) = (adapter.as_mut(), link.as_ref()) {
            if let Some(next) = a.poll(l, &wire, sink.total) {
                enc_cfg.bitrate_bps = next.mbps * 1_000_000;
                interval_us.store(1_000_000 / next.fps.max(1) as u64, Ordering::Relaxed);
                bitrate_check = None;
                if next.fps != target.fps || dynamic_ignored || !encoder.set_bitrate(enc_cfg.bitrate_bps) {
                    reopen = true;
                } else {
                    bitrate_check = Some((next.mbps, Instant::now() + BITRATE_CHECK_AFTER));
                }
                target = next;
                let mut s = stats.lock().unwrap();
                (s.target_mbps, s.target_fps, s.adapt_reason) = (target.mbps, target.fps, a.ctl.reason());
            }
        }
        // The capture side cannot feed the GPU path (other adapter, shared texture failure).
        if pipe.gpu && counters.gpu_failed.load(Ordering::Relaxed) {
            pipe.drop_gpu("capture side", &mut enc_cfg);
            reopen |= encoder.gpu_input;
        }
        if reopen {
            // A new encoder starts with an IDR, so the TV resyncs by itself.
            enc_cfg.fps = target.fps;
            enc_cfg.gop_frames = gop(target.fps);
            let name = encoder.name.clone();
            drop(encoder); // free the hardware session before opening the next one
            encoder = open_encoder(&mut enc_cfg, Some(&name), &bad_encoders, &mut fallback)?;
            pipe.follow(&encoder);
            stats.lock().unwrap().encoder = encoder.name.clone();
        }

        let step = match rx.recv_timeout(Duration::from_millis(5)) {
            // Frames captured just before a pause are not sent.
            Ok(_) if held => {
                pipe.force_next.store(true, Ordering::Relaxed);
                Ok(())
            }
            Ok(msg) => {
                // keyframe_request from the receiver (set by the control reader thread).
                if want_keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                pipe.encode(&mut encoder, &mut sink, msg, t0).map(|us| {
                    if let Some(us) = us {
                        convert_us += us;
                        converted += 1;
                    }
                })
            }
            Err(RecvTimeoutError::Timeout) if !held => {
                encoder.poll_output(|f| sink.push(f)).and_then(|()| pipe.repeat_if_due(&mut encoder, &mut sink, t0))
            }
            Err(RecvTimeoutError::Timeout) => encoder.poll_output(|f| sink.push(f)),
            Err(RecvTimeoutError::Disconnected) => break EndReason::SourceClosed,
        };
        if let Err(e) = step {
            // GPU input first falls back to CPU input with the same encoder.
            if encoder.gpu_input {
                pipe.drop_gpu(&format!("{e:#}"), &mut enc_cfg);
                let name = encoder.name.clone();
                drop(encoder);
                encoder = open_encoder(&mut enc_cfg, Some(&name), &bad_encoders, &mut fallback)?;
                pipe.follow(&encoder);
                stats.lock().unwrap().encoder = encoder.name.clone();
                continue;
            }
            // An encoder that opened but never produced a frame is treated like one that
            // failed to open: try the next one. After the first frame, an error is fatal.
            if sink.total > 0 {
                return Err(e).context(format!("encoder {}", encoder.name));
            }
            warn!("encoder {} failed before its first frame ({e:#}); trying the next one", encoder.name);
            bad_encoders.push(encoder.name.clone());
            drop(encoder);
            encoder = open_encoder(&mut enc_cfg, None, &bad_encoders, &mut fallback)?;
            pipe.follow(&encoder);
            let mut s = stats.lock().unwrap();
            (s.encoder, s.codec, s.codec_fallback) = (encoder.name.clone(), enc_cfg.codec, fallback);
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
            s.frames_dropped = counters.dropped.load(Ordering::Relaxed) as u64 + sink.net.dropped;
            s.frames_unchanged = counters.unchanged.load(Ordering::Relaxed) as u64;
            s.frames_repeated = pipe.repeated;
            s.gpu_convert = encoder.gpu_input && pipe.gpu;
            (s.paused, s.blank) = (paused, blank);
            if s.gpu_convert && sink.frames > 0 && s.latency_ms > GPU_SLOW_MS {
                gpu_slow += 1;
                if gpu_slow >= GPU_SLOW_WINDOWS {
                    let why = format!("capture->encoded {:.0} ms for {GPU_SLOW_WINDOWS} windows", s.latency_ms);
                    pipe.drop_gpu(&why, &mut enc_cfg);
                    reopen_pending = true;
                }
            } else {
                gpu_slow = 0;
            }
            // Judged only while frames flow (a static screen is below any target anyway).
            if let Some((want, at)) = bitrate_check {
                if Instant::now() >= at && s.fps >= 20.0 {
                    bitrate_check = None;
                    if s.mbps > want as f64 * 1.25 {
                        warn!(
                            "encoder {} accepted {want} Mbps on the fly but still sends {:.1} Mbps; reopening it for every change",
                            encoder.name, s.mbps
                        );
                        dynamic_ignored = true;
                        reopen_pending = true;
                    } else {
                        info!("encoder {}: on-the-fly bitrate verified ({:.1} Mbps for {want})", encoder.name, s.mbps);
                    }
                }
            }
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

    // Release in order: capture and audio, network, encoder, then `stop` to the receiver.
    drop(capture);
    drop(audio.sender.take());
    drop(sink);
    drop(encoder);
    if let Some(l) = link.take() {
        l.close();
    }
    Ok(reason)
}

fn encode_one(enc: &mut MfEncoder, sink: &mut Sink, nv12: &[u8], pts_hns: i64, captured: Instant) -> Result<()> {
    enc.wait_need_input(|f| sink.push(f))?;
    enc.submit(nv12, pts_hns, captured)?;
    enc.poll_output(|f| sink.push(f))
}

fn encode_texture(
    enc: &mut MfEncoder,
    sink: &mut Sink,
    frame: &GpuFrame,
    pts_hns: i64,
    captured: Instant,
) -> Result<()> {
    enc.wait_need_input(|f| sink.push(f))?;
    enc.submit_texture(&frame.tex, frame.slot, pts_hns, captured)?;
    enc.poll_output(|f| sink.push(f))
}
