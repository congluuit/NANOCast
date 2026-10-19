//! One cast: capture -> NV12 -> hardware H.264 -> paced UDP to one receiver, plus the
//! control channel. Runs on its own thread; the caller (GUI) only starts it, reads
//! `stats()` and stops it. Every resource is released by RAII guards in reverse order:
//! capture stops first, then the network thread, the encoder, the control channel
//! (`stop` is sent), and finally Media Foundation.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use log::{error, info, warn};
use nanocast_core::control::Message;
use nanocast_core::protocol::{PacedSender, MAX_PAYLOAD};

use crate::capture::{self, CaptureCounters, CaptureFlags, CapturedFrame, Source};
use crate::control_link::{ControlLink, ReceiverReport};
use crate::convert::fit_size;
use crate::mf_encoder::{EncodedFrame, EncoderConfig, MfEncoder, MfRuntime};
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
    pub bitrate_mbps: u32,
    /// Encoder friendly name to try first; `None` = merit order.
    pub encoder: Option<String>,
    /// Packet pacing ceiling (PLAN.md section 4: default 80 Mbps).
    pub pace_mbps: u64,
    /// Shown on the TV (`hello.name`).
    pub sender_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// `stop()` was called.
    Stopped,
    /// The captured window was closed (or the monitor went away).
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
    pub convert_ms: f64,
    /// Capture -> encoded frame ready to send.
    pub latency_ms: f64,
    pub frames_sent: u64,
    pub keyframes: u64,
    /// Frames skipped because the encoder or the network was busy.
    pub frames_dropped: u64,
    pub keyframe_requests: usize,
    /// `None` when the receiver has no control channel (UDP only).
    pub receiver: Option<ReceiverReport>,
}

pub struct CastSession {
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<CastStats>>,
    thread: Option<JoinHandle<()>>,
}

impl CastSession {
    /// Starts casting in the background. Failures show up as `Phase::Failed` in `stats()`.
    pub fn start(cfg: CastConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(CastStats::default()));
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name("cast".into())
                .spawn(move || {
                    let phase = match run(&cfg, &stop, &stats) {
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
        Self { stop, stats, thread: Some(thread) }
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

fn run(cfg: &CastConfig, stop: &AtomicBool, stats: &Mutex<CastStats>) -> Result<EndReason> {
    let _rt = MfRuntime::init()?;

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

    // Fixed encoder size: a monitor keeps its own size (capped); a window is scaled into
    // a full TV frame so resizing it never needs a new stream.
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
        encoder_index: 0,
    };
    let mut bad_encoders = Vec::new();
    let mut encoder = MfEncoder::open_first_working(&enc_cfg, cfg.encoder.as_deref(), &bad_encoders)?;
    let start_msg = Message::Start { w: canvas.0, h: canvas.1, fps: cfg.fps, codec: "h264".into(), audio: false };
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

    let (tx, rx): (SyncSender<CapturedFrame>, Receiver<CapturedFrame>) = sync_channel(2);
    let counters = Arc::new(CaptureCounters::default());
    let flags = CaptureFlags {
        tx,
        min_interval: Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64),
        canvas,
        counters: counters.clone(),
    };
    let capture = CaptureGuard(Some(capture::start(&cfg.source, flags)?));
    stats.lock().unwrap().phase = Phase::Casting;
    info!("casting {:?} -> {} as {}x{} @ {} fps, {} Mbps", cfg.source, cfg.video_addr, canvas.0, canvas.1, cfg.fps, cfg.bitrate_mbps);

    let t0 = Instant::now();
    let mut last_publish = Instant::now();
    let mut convert_us = 0u64;
    let mut converted = 0u64;
    let mut last_alive_check = Instant::now();

    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break EndReason::Stopped;
        }
        if counters.closed.load(Ordering::Relaxed) || capture.0.as_ref().is_some_and(|h| h.is_finished()) {
            break EndReason::SourceClosed;
        }
        if let Source::Window(hwnd) = cfg.source {
            if last_alive_check.elapsed() >= Duration::from_secs(1) {
                last_alive_check = Instant::now();
                if !sources::window_alive(hwnd) {
                    break EndReason::SourceClosed;
                }
            }
        }

        let step = match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(msg) => {
                convert_us += msg.convert_us;
                converted += 1;
                // keyframe_request from the receiver (set by the control reader thread).
                if want_keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                let pts_hns = msg.captured.duration_since(t0).as_nanos() as i64 / 100;
                encode_one(&mut encoder, &mut sink, &msg.nv12, pts_hns, msg.captured)
            }
            Err(RecvTimeoutError::Timeout) => encoder.poll_output(|f| sink.push(f)),
            Err(RecvTimeoutError::Disconnected) => break EndReason::SourceClosed,
        };
        if let Err(e) = step {
            // An encoder that opened but never produced a frame is treated like one that
            // failed to open: try the next one. After the first frame, an error is fatal.
            if sink.total > 0 {
                return Err(e).context(format!("encoder {}", encoder.name));
            }
            warn!("encoder {} failed before its first frame ({e:#}); trying the next one", encoder.name);
            bad_encoders.push(encoder.name.clone());
            encoder = MfEncoder::open_first_working(&enc_cfg, None, &bad_encoders)?;
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
            s.frames_dropped = counters.dropped.load(Ordering::Relaxed) as u64 + sink.net.dropped;
            if let Some(l) = &link {
                s.keyframe_requests = l.keyframe_requests.load(Ordering::Relaxed);
                s.receiver = Some(l.report.lock().unwrap().clone());
            }
            drop(s);
            (sink.frames, sink.bytes, sink.encode_us, sink.latency_us, convert_us, converted) = (0, 0, 0, 0, 0, 0);
            last_publish = Instant::now();
        }
    };

    // Release in order: capture, network, encoder, then `stop` to the receiver.
    drop(capture);
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
