//! Windows Graphics Capture of a monitor or one window, paced to the target fps and
//! converted to NV12 at a fixed canvas size (the encoder size never changes mid-stream;
//! a resized window is scaled to fit with black bars).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
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

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// One-based monitor index (see `sources::list_monitors`).
    Monitor(usize),
    /// Raw HWND of a top-level window (see `sources::list_windows`).
    Window(isize),
}

pub struct CapturedFrame {
    pub nv12: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured: Instant,
    pub convert_us: u64,
}

#[derive(Default)]
pub struct CaptureCounters {
    pub captured: AtomicUsize,
    /// Frames dropped because the encoder was still busy.
    pub dropped: AtomicUsize,
    /// Set when the captured item went away (window closed, monitor unplugged).
    pub closed: AtomicBool,
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
}

pub struct Capture {
    flags: CaptureFlags,
    scratch: Vec<u8>,
    next_due: Option<Instant>,
}

pub type Handle = CaptureControl<Capture, <Capture as GraphicsCaptureApiHandler>::Error>;

impl GraphicsCaptureApiHandler for Capture {
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, scratch: Vec::new(), next_due: None })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        let captured = Instant::now();
        if self.flags.paused.load(Ordering::Relaxed) {
            self.next_due = None;
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
        if src_w < 16 || src_h < 16 {
            return Ok(());
        }
        let buffer = frame.buffer()?;
        let bgra = buffer.as_nopadding_buffer(&mut self.scratch);

        let (w, h) = self.flags.canvas;
        let t = Instant::now();
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        convert::bgra_to_nv12_fit(bgra, src_w, src_h, w, h, &mut nv12);
        let convert_us = t.elapsed().as_micros() as u64;

        self.flags.counters.captured.fetch_add(1, Ordering::Relaxed);
        match self.flags.tx.try_send(CapturedFrame { nv12, width: w, height: h, captured, convert_us }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.flags.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => control.stop(),
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
