//! Windows Graphics Capture of a monitor or one window, paced to the target fps, with
//! unchanged frames left out (GPU tile hash, `gpu::TileHasher`). Each frame is then
//! converted to NV12 here, either
//! - GPU mode: by the video processor into a texture on the capture device, which the
//!   encoder reads directly (`gpu::Nv12Converter`), or
//! - CPU mode: read back and converted with rayon,
//!
//! always at a fixed canvas size (the encoder size never changes mid-stream; a resized window
//! is scaled to fit with black bars).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use log::warn;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, GraphicsCaptureItemType,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::convert;
use crate::gpu::{self, GpuFrame, Nv12Converter, TileHasher};

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// One-based monitor index (see `sources::list_monitors`).
    Monitor(usize),
    /// Raw HWND of a top-level window (see `sources::list_windows`).
    Window(isize),
}

/// Values of `CaptureFlags::mode`.
pub const MODE_CPU: u8 = 0;
pub const MODE_GPU: u8 = 1;
/// No encoder yet: frames are dropped before any work.
pub const MODE_WAIT: u8 = 2;

pub enum Pixels {
    /// NV12 at the canvas size.
    Cpu(Vec<u8>),
    /// NV12 at the canvas size in a texture of the capture device (reserved until dropped).
    Gpu(GpuFrame),
}

pub struct CapturedFrame {
    pub pixels: Pixels,
    pub captured: Instant,
    /// Time spent on this frame on the capture thread (hash + video processor, or read back +
    /// convert).
    pub convert_us: u64,
}

#[derive(Default)]
pub struct CaptureCounters {
    pub captured: AtomicUsize,
    /// Frames dropped because the encoder was still busy.
    pub dropped: AtomicUsize,
    /// Set when the captured item went away (window closed, monitor unplugged).
    pub closed: AtomicBool,
    /// Frames left out because nothing on screen changed.
    pub unchanged: AtomicUsize,
    /// The GPU conversion failed while capturing: the session switches to `MODE_CPU`.
    pub gpu_failed: AtomicBool,
}

pub struct CaptureFlags {
    pub tx: SyncSender<CapturedFrame>,
    /// Minimum spacing between accepted frames in microseconds (pacing to the target fps);
    /// shared so the session can change the frame rate while capturing.
    pub interval_us: Arc<AtomicU64>,
    /// While set, frames are dropped before conversion (pause / blank: no CPU spent).
    pub paused: Arc<AtomicBool>,
    /// Output (encoder) size; frames of another size are scaled to fit.
    pub canvas: (u32, u32),
    pub counters: Arc<CaptureCounters>,
    /// `MODE_*`; the session changes it when the encoder changes.
    pub mode: Arc<AtomicU8>,
    /// Asks for the GPU path: the capture device is sent here once it is made multithread
    /// protected and a video processor for it exists (`None`: GPU path unavailable).
    pub gpu_tx: Option<SyncSender<Option<ID3D11Device>>>,
    /// Leave out frames identical to the previous one (the session repeats the last frame
    /// at least once a second by itself).
    pub skip_static: bool,
    /// Set by the session when it dropped a frame: the next one is sent even if unchanged.
    pub force_next: Arc<AtomicBool>,
}

pub struct Capture {
    flags: CaptureFlags,
    scratch: Vec<u8>,
    next_due: Option<Instant>,
    hasher: Option<TileHasher>,
    converter: Option<Nv12Converter>,
    /// Previous BGRA frame, for change detection on the CPU when the hash shader is unavailable.
    prev_bgra: Vec<u8>,
    /// The previous frame never reached the encoder: send the next one whatever it shows.
    resync: bool,
}

pub type Handle = CaptureControl<Capture, <Capture as GraphicsCaptureApiHandler>::Error>;

impl Capture {
    fn drop_frame(&mut self) {
        self.flags.counters.dropped.fetch_add(1, Ordering::Relaxed);
        self.resync = true;
    }

    /// Hands a frame to the session; `false` when the session is gone.
    fn send(&mut self, frame: CapturedFrame) -> bool {
        match self.flags.tx.try_send(frame) {
            Ok(()) => true,
            // Dropping the frame also frees its GPU texture.
            Err(TrySendError::Full(_)) => {
                self.drop_frame();
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let flags = ctx.flags;
        let converter = flags.gpu_tx.as_ref().and_then(|tx| {
            let conv = gpu::protect(&ctx.device_context)
                .and_then(|_| Nv12Converter::new(&ctx.device, &ctx.device_context, flags.canvas))
                .inspect_err(|e| warn!("GPU conversion unavailable ({e:#}); converting on the CPU"))
                .ok();
            let _ = tx.try_send(conv.as_ref().map(|_| ctx.device.clone()));
            conv
        });
        let hasher = if flags.skip_static {
            TileHasher::new(&ctx.device, &ctx.device_context)
                .inspect_err(|e| warn!("GPU change detection unavailable ({e:#}); comparing on the CPU"))
                .ok()
        } else {
            None
        };
        Ok(Self {
            flags,
            scratch: Vec::new(),
            next_due: None,
            hasher,
            converter,
            prev_bgra: Vec::new(),
            resync: true,
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        let captured = Instant::now();
        if self.flags.paused.load(Ordering::Relaxed) {
            self.next_due = None;
            self.resync = true;
            return Ok(());
        }
        // Pace to the target fps: a 144 Hz monitor delivers up to 144 frames/s.
        // Keep a running "next due" time so the average rate is exact even when
        // the monitor period does not divide the target period.
        let interval = Duration::from_micros(self.flags.interval_us.load(Ordering::Relaxed).max(1));
        match self.next_due {
            Some(due) if captured + Duration::from_millis(1) < due => return Ok(()),
            Some(due) => {
                let mut next = due + interval;
                if next < captured || next > captured + interval {
                    next = captured + interval; // fell behind or the rate changed: resync
                }
                self.next_due = Some(next);
            }
            None => self.next_due = Some(captured + interval),
        }
        let (src_w, src_h) = (frame.width(), frame.height());
        let mode = self.flags.mode.load(Ordering::Relaxed);
        if src_w < 16 || src_h < 16 || mode == MODE_WAIT {
            return Ok(());
        }

        let resync = std::mem::take(&mut self.resync) | self.flags.force_next.swap(false, Ordering::Relaxed);
        let t = Instant::now();
        let texture = frame.as_raw_texture().clone();
        if let Some(hasher) = self.hasher.as_mut() {
            match hasher.changed(&texture, src_w, src_h) {
                Ok(false) if !resync => {
                    self.flags.counters.unchanged.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("GPU change detection failed ({e:#}); comparing on the CPU");
                    self.hasher = None;
                }
            }
        }

        let pixels = if let (MODE_GPU, Some(conv)) = (mode, self.converter.as_mut()) {
            match conv.convert(&texture, src_w, src_h) {
                Ok(Some(f)) => Pixels::Gpu(f),
                Ok(None) => {
                    self.drop_frame();
                    return Ok(());
                }
                Err(e) => {
                    warn!("GPU conversion failed ({e:#}); converting on the CPU");
                    self.converter = None;
                    self.flags.counters.gpu_failed.store(true, Ordering::Relaxed);
                    self.drop_frame();
                    return Ok(());
                }
            }
        } else if mode == MODE_GPU {
            // Waiting for the session to switch to CPU input.
            self.drop_frame();
            return Ok(());
        } else {
            let buffer = frame.buffer()?;
            let bgra = buffer.as_nopadding_buffer(&mut self.scratch);
            if self.flags.skip_static && self.hasher.is_none() {
                if !resync && bgra == self.prev_bgra.as_slice() {
                    self.flags.counters.unchanged.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                self.prev_bgra.clear();
                self.prev_bgra.extend_from_slice(bgra);
            }
            let (w, h) = self.flags.canvas;
            let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
            convert::bgra_to_nv12_fit(bgra, src_w, src_h, w, h, &mut nv12);
            Pixels::Cpu(nv12)
        };
        let convert_us = t.elapsed().as_micros() as u64;

        self.flags.counters.captured.fetch_add(1, Ordering::Relaxed);
        if !self.send(CapturedFrame { pixels, captured, convert_us }) {
            control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.flags.counters.closed.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Size in pixels of what `source` shows right now (window: outer rectangle).
pub fn source_size(source: &Source) -> Result<(u32, u32)> {
    match *source {
        Source::Monitor(i) => {
            let m = Monitor::from_index(i).map_err(|e| anyhow!("monitor {i}: {e}"))?;
            Ok((m.width()?, m.height()?))
        }
        Source::Window(hwnd) => {
            let w = Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void);
            Ok((w.width()?.max(0) as u32, w.height()?.max(0) as u32))
        }
    }
}

/// Starts capturing on windows-capture's own thread. Stop it with `Handle::stop()`.
pub fn start(source: &Source, flags: CaptureFlags) -> Result<Handle> {
    match *source {
        Source::Monitor(i) => {
            let m = Monitor::from_index(i).map_err(|e| anyhow!("monitor {i}: {e}"))?;
            start_item(m, flags)
        }
        Source::Window(hwnd) => {
            let w = Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("the window is gone"));
            }
            start_item(w, flags)
        }
    }
}

fn start_item<T>(item: T, flags: CaptureFlags) -> Result<Handle>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        item,
        CursorCaptureSettings::Default,
        // Windows 10 cannot hide the yellow border; Windows 11 could.
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        // Custom intervals need Windows 11; pacing is done in on_frame_arrived instead.
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    Capture::start_free_threaded(settings).map_err(|e| anyhow!("capture start: {e:?}")).context("Windows Graphics Capture")
}
