//! Picture source for Google Cast on Windows, built from nanocast-win's pieces:
//! Windows Graphics Capture -> NV12 (GPU video processor, CPU fallback) -> Media Foundation
//! hardware H.264 (Main profile, CBR, no B-frames, keyframe every `gop_frames`).
//! A simplified `nanocast_win::session::run` without the NANOCast control channel.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use log::{info, warn};
use nanocast_core::protocol::VideoCodec;
use nanocast_win::capture::{self, CaptureCounters, CaptureFlags, CapturedFrame, Pixels, Source, MODE_CPU, MODE_GPU, MODE_WAIT};
use nanocast_win::convert::fit_size;
use nanocast_win::gpu::{device_manager, GpuFrame};
use nanocast_win::mf_encoder::{EncodedFrame, EncoderConfig, MfEncoder};
use nanocast_win::sources;

use crate::{CastConfig, SourceSpec};

/// Longest gap between two frames while the screen does not change: players and the
/// Cast receiver keep a steady clock.
const REPEAT_AFTER: Duration = Duration::from_millis(250);

pub struct VideoFrame {
    /// Annex B access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture time since the session's `t0`.
    pub pts_us: u64,
    pub captured: Instant,
}

#[derive(Default)]
pub struct VideoCtl {
    pub stop: AtomicBool,
    pub want_keyframe: AtomicBool,
    /// Size actually encoded, set once the encoder is open (w << 32 | h).
    pub size: AtomicU64,
}

/// `SourceSpec` -> nanocast-win `Source`: monitor 0 = primary, n = one-based index as in
/// `nanocast_win::sources::list_monitors`.
pub fn resolve_source(spec: &SourceSpec) -> Result<Source> {
    Ok(match *spec {
        SourceSpec::Monitor(0) => {
            let monitors = sources::list_monitors();
            let m = monitors.iter().find(|m| m.primary).or(monitors.first()).context("không tìm thấy màn hình nào")?;
            Source::Monitor(m.index)
        }
        SourceSpec::Monitor(i) => Source::Monitor(i),
        SourceSpec::Window(h) => Source::Window(h as isize),
    })
}

struct CaptureGuard(Option<capture::Handle>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = h.stop();
        }
    }
}

enum Last {
    Cpu(Vec<u8>),
    Gpu(GpuFrame),
}

/// Runs capture + encoder on the calling thread until `ctl.stop`; every encoded frame goes
/// to `on_frame`. Media Foundation must be initialised on this thread.
pub fn run(
    spec: &SourceSpec,
    cfg: &CastConfig,
    gop_frames: u32,
    t0: Instant,
    ctl: &VideoCtl,
    mut on_frame: impl FnMut(VideoFrame),
) -> Result<()> {
    let source = resolve_source(spec)?;
    let canvas = match &source {
        Source::Monitor(_) => {
            let (w, h) = capture::source_size(&source)?;
            fit_size(w, h, cfg.width, cfg.height)
        }
        Source::Window(_) => (cfg.width & !1, cfg.height & !1),
    };
    let fps = cfg.fps.clamp(1, 60);

    let (tx, rx) = sync_channel::<CapturedFrame>(2);
    let (gpu_tx, gpu_rx) = sync_channel(1);
    let counters = Arc::new(CaptureCounters::default());
    let mode = Arc::new(AtomicU8::new(MODE_WAIT));
    let force_next = Arc::new(AtomicBool::new(false));
    let want_gpu = std::env::var("NANOCAST_CONVERT").map(|v| v != "cpu").unwrap_or(true);
    let flags = CaptureFlags {
        tx,
        interval_us: Arc::new(AtomicU64::new(1_000_000 / fps as u64)),
        paused: Arc::new(AtomicBool::new(false)),
        canvas,
        counters: counters.clone(),
        mode: mode.clone(),
        gpu_tx: want_gpu.then_some(gpu_tx),
        skip_static: true,
        force_next: force_next.clone(),
    };
    let capture = CaptureGuard(Some(capture::start(&source, flags).context("không chụp được màn hình / cửa sổ")?));
    let d3d = if want_gpu {
        match gpu_rx.recv_timeout(Duration::from_secs(3)).ok().flatten().map(|d| device_manager(&d)) {
            Some(Ok(m)) => Some(m),
            Some(Err(e)) => {
                warn!("GPU conversion unavailable ({e:#}); converting on the CPU");
                None
            }
            None => None,
        }
    } else {
        None
    };
    let mut enc_cfg = EncoderConfig {
        codec: VideoCodec::H264,
        width: canvas.0,
        height: canvas.1,
        fps,
        bitrate_bps: cfg.bitrate_bps,
        gop_frames,
        encoder_index: 0,
        d3d,
    };
    let mut skip = Vec::new();
    let mut enc = MfEncoder::open_first_working(&enc_cfg, None, &skip).context("máy tính không có bộ mã hoá H.264 phần cứng dùng được")?;
    let mut gpu = enc.gpu_input && enc_cfg.d3d.is_some();
    mode.store(if gpu { MODE_GPU } else { MODE_CPU }, Ordering::Relaxed);
    force_next.store(true, Ordering::Relaxed);
    ctl.size.store((canvas.0 as u64) << 32 | canvas.1 as u64, Ordering::Relaxed);
    info!("cast video: {}x{} @ {fps} fps, {} kbps, GOP {gop_frames}, {} ({})", canvas.0, canvas.1, cfg.bitrate_bps / 1000, enc.name, if gpu { "GPU" } else { "CPU" });

    let mut emit = |f: EncodedFrame| {
        on_frame(VideoFrame {
            data: f.data,
            keyframe: f.keyframe,
            pts_us: (f.pts_hns.max(0) / 10) as u64,
            captured: f.capture_instant,
        })
    };
    let mut last: Option<Last> = None;
    let mut encoding: Vec<GpuFrame> = Vec::new();
    let mut last_input = Instant::now();
    let mut last_alive = Instant::now();

    loop {
        if ctl.stop.load(Ordering::Relaxed) {
            break;
        }
        if counters.closed.load(Ordering::Relaxed) || capture.0.as_ref().is_some_and(|h| h.is_finished()) {
            bail!("cửa sổ / màn hình đang chiếu đã đóng");
        }
        if let Source::Window(hwnd) = source {
            if last_alive.elapsed() >= Duration::from_secs(1) {
                last_alive = Instant::now();
                if !sources::window_alive(hwnd) {
                    bail!("cửa sổ đang chiếu đã đóng");
                }
            }
        }
        if ctl.want_keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }
        let step: Result<()> = (|| {
            match rx.recv_timeout(Duration::from_millis(4)) {
                Ok(frame) => {
                    let pts = frame.captured.duration_since(t0).as_nanos() as i64 / 100;
                    match frame.pixels {
                        Pixels::Cpu(nv12) if !enc.gpu_input => {
                            enc.wait_need_input(&mut emit)?;
                            enc.submit(&nv12, pts, frame.captured)?;
                            last = Some(Last::Cpu(nv12));
                        }
                        Pixels::Gpu(f) if enc.gpu_input && gpu => {
                            enc.wait_need_input(&mut emit)?;
                            enc.submit_texture(&f.tex, f.slot, pts, frame.captured)?;
                            if let Some(Last::Gpu(prev)) = last.replace(Last::Gpu(f)) {
                                encoding.push(prev);
                            }
                        }
                        _ => {
                            force_next.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                    }
                    last_input = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {
                    if last_input.elapsed() >= REPEAT_AFTER {
                        let now = Instant::now();
                        let pts = now.duration_since(t0).as_nanos() as i64 / 100;
                        match &last {
                            Some(Last::Cpu(nv12)) if !enc.gpu_input => {
                                enc.wait_need_input(&mut emit)?;
                                enc.submit(nv12, pts, now)?;
                                last_input = now;
                            }
                            Some(Last::Gpu(f)) if enc.gpu_input && !enc.slot_busy(f.slot) => {
                                enc.wait_need_input(&mut emit)?;
                                enc.submit_texture(&f.tex, f.slot, pts, now)?;
                                last_input = now;
                            }
                            _ => {}
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => bail!("capture ended"),
            }
            enc.poll_output(&mut emit)?;
            encoding.retain(|f| enc.slot_busy(f.slot));
            Ok(())
        })();
        let failed_gpu = gpu && counters.gpu_failed.load(Ordering::Relaxed);
        if step.is_err() || failed_gpu {
            // GPU input first falls back to CPU input; then the next encoder is tried.
            let why = step.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "capture side GPU failure".into());
            let name = enc.name.clone();
            let preferred = if gpu {
                warn!("cast video: GPU path off ({why})");
                gpu = false;
                enc_cfg.d3d = None;
                Some(name.as_str())
            } else {
                warn!("cast video: encoder {name} failed ({why}); trying the next one");
                skip.push(name.clone());
                if skip.len() > 3 {
                    bail!("bộ mã hoá H.264 lỗi: {why}");
                }
                None
            };
            last = None;
            encoding.clear();
            drop(enc);
            enc = MfEncoder::open_first_working(&enc_cfg, preferred, &skip)?;
            mode.store(MODE_CPU, Ordering::Relaxed);
            force_next.store(true, Ordering::Relaxed);
        }
    }
    drop(capture);
    Ok(())
}
