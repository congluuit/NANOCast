//! Screen or window capture on Linux, paced to the target fps and converted to NV12 at a
//! fixed canvas size (the encoder size never changes mid-stream; a resized window is scaled
//! to fit with black bars). Two back ends:
//!
//! - **Portal** (`portal.rs` + `pw_video.rs`, feature `pipewire`): xdg-desktop-portal ScreenCast. Works on
//!   Wayland (GNOME, KDE, wlroots with xdg-desktop-portal-wlr, ...) and on X11 desktops
//!   whose portal implements it. The system dialog chooses the screen or window; NANOCast
//!   cannot preselect it. Frames arrive over PipeWire (SHM/memfd, or DMA-BUF mapped when
//!   linear).
//! - **X11** (`x11.rs`): MIT-SHM `GetImage` of a RandR monitor, or of one window through the
//!   Composite extension (content even when covered), cursor drawn with XFixes.
//!
//! `backend()` picks one: `NANOCAST_CAPTURE=portal|x11` forces it; otherwise X11 sessions
//! use X11 (exact monitor/window choice, no dialog) and everything else uses the portal.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

/// What to capture (same shape as `nanocast_win::capture::Source`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// One-based monitor index (see `sources::list_monitors`). With the portal the system
    /// dialog still asks which screen; the index only keys the remembered choice.
    Monitor(usize),
    /// X11 window id (see `sources::list_windows`), or `PORTAL_WINDOW` to let the portal
    /// dialog pick the window (Wayland cannot enumerate other apps' windows).
    Window(isize),
}

/// `Source::Window(PORTAL_WINDOW)`: "choose the window in the system dialog".
pub const PORTAL_WINDOW: isize = 0;

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
    /// Set when the captured item went away (window closed, sharing stopped from the
    /// desktop's indicator, monitor unplugged, PipeWire stream error).
    pub closed: AtomicBool,
}

pub struct CaptureFlags {
    pub tx: SyncSender<CapturedFrame>,
    /// Minimum spacing between accepted frames in microseconds (pacing to the target fps;
    /// the adaptive bitrate changes it on the fly).
    pub interval_us: Arc<AtomicU64>,
    /// Pause / blank: nothing is grabbed or converted while set.
    pub paused: Arc<AtomicBool>,
    /// Output (encoder) size; frames of another size are scaled to fit.
    pub canvas: (u32, u32),
    pub counters: Arc<CaptureCounters>,
}

impl CaptureFlags {
    pub(crate) fn interval(&self) -> Duration {
        Duration::from_micros(self.interval_us.load(Ordering::Relaxed).max(1))
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Hands a converted frame to the encoder loop; drops it when the loop is busy.
    /// Returns false once the receiving side is gone.
    pub(crate) fn deliver(&self, frame: CapturedFrame) -> bool {
        self.counters.captured.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Paces frames to the target fps. Keeps a running "next due" time so the average rate is
/// exact even when the source period does not divide the target period (same rule as the
/// Windows capture).
#[derive(Default)]
pub(crate) struct Pacer {
    next_due: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Forget the schedule (after a pause).
    pub(crate) fn reset(&mut self) {
        self.next_due = None;
    }

    /// Whether a frame arriving at `now` should be kept, for a target spacing of `interval`.
    pub(crate) fn accept(&mut self, now: Instant, interval: Duration) -> bool {
        match self.next_due {
            Some(due) if now + Duration::from_millis(1) < due => false,
            Some(due) => {
                let mut next = due + interval;
                if next < now || next > now + interval {
                    next = now + interval; // fell behind or the rate changed: resync
                }
                self.next_due = Some(next);
                true
            }
            None => {
                self.next_due = Some(now + interval);
                true
            }
        }
    }

    /// Sleeps until the next frame is due (pull sources: X11), at most `max`.
    pub(crate) fn wait(&mut self, max: Duration) {
        if let Some(due) = self.next_due {
            let now = Instant::now();
            if due > now {
                std::thread::sleep((due - now).min(max));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// xdg-desktop-portal ScreenCast + PipeWire.
    Portal,
    /// X11 MIT-SHM / Composite.
    X11,
}

/// True on a Wayland session (the X11 back end would only see XWayland apps there).
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"))
}

/// The capture back end for this session; see the module docs.
pub fn backend() -> Backend {
    let portal_built = cfg!(feature = "pipewire");
    match std::env::var("NANOCAST_CAPTURE").map(|v| v.to_ascii_lowercase()) {
        Ok(v) if v == "x11" => return Backend::X11,
        Ok(v) if v == "portal" && portal_built => return Backend::Portal,
        _ => {}
    }
    if !portal_built {
        return Backend::X11;
    }
    if !is_wayland() && std::env::var_os("DISPLAY").is_some() {
        Backend::X11
    } else {
        Backend::Portal
    }
}

/// A source opened but not streaming yet: the portal dialog has been answered (or the X11
/// target found), so the picture size is known and the canvas can be chosen.
pub struct Prepared {
    inner: PreparedInner,
}

enum PreparedInner {
    #[cfg(feature = "pipewire")]
    Portal(crate::pw_video::Prepared),
    X11(Box<crate::x11::Target>),
}

/// Opens `source` with the session's back end. With the portal this shows the system
/// dialog (unless a remembered choice applies) and blocks until it is answered; `stop`
/// aborts the wait.
pub fn prepare(source: &Source, stop: &AtomicBool) -> Result<Prepared> {
    let inner = match backend() {
        #[cfg(feature = "pipewire")]
        Backend::Portal => PreparedInner::Portal(crate::pw_video::prepare(source, stop)?),
        _ => {
            let _ = stop;
            PreparedInner::X11(Box::new(crate::x11::Target::open(source)?))
        }
    };
    Ok(Prepared { inner })
}

impl Prepared {
    /// Picture size in pixels as the source reports it now.
    pub fn size(&self) -> (u32, u32) {
        match &self.inner {
            #[cfg(feature = "pipewire")]
            PreparedInner::Portal(p) => p.size(),
            PreparedInner::X11(t) => t.size(),
        }
    }

    pub fn backend(&self) -> Backend {
        match &self.inner {
            #[cfg(feature = "pipewire")]
            PreparedInner::Portal(_) => Backend::Portal,
            PreparedInner::X11(_) => Backend::X11,
        }
    }

    /// Starts delivering frames on a capture thread.
    pub fn start(self, flags: CaptureFlags) -> Result<Handle> {
        let inner = match self.inner {
            #[cfg(feature = "pipewire")]
            PreparedInner::Portal(p) => HandleInner::Portal(p.start(flags)?),
            PreparedInner::X11(t) => HandleInner::X11(t.start(flags)?),
        };
        Ok(Handle { inner })
    }
}

/// A running capture; stopped (and its thread joined) by `stop` or on drop.
pub struct Handle {
    inner: HandleInner,
}

enum HandleInner {
    #[cfg(feature = "pipewire")]
    Portal(crate::pw_video::Handle),
    X11(crate::x11::Handle),
}

impl Handle {
    /// True once the source went away (window closed, sharing stopped...).
    pub fn is_finished(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "pipewire")]
            HandleInner::Portal(h) => h.is_finished(),
            HandleInner::X11(h) => h.is_finished(),
        }
    }

    pub fn stop(self) -> Result<()> {
        match self.inner {
            #[cfg(feature = "pipewire")]
            HandleInner::Portal(h) => h.stop(),
            HandleInner::X11(h) => h.stop(),
        }
    }
}

/// Size in pixels of what `source` shows right now. X11 back end only: the portal knows
/// the size after its dialog (`prepare(..).size()`), so this fails there.
pub fn source_size(source: &Source) -> Result<(u32, u32)> {
    match backend() {
        Backend::X11 => Ok(crate::x11::Target::open(source)?.size()),
        Backend::Portal => anyhow::bail!("the portal reports the size only after its dialog; use capture::prepare"),
    }
}

/// Opens and starts in one step (same as the Windows/macOS `capture::start`).
pub fn start(source: &Source, flags: CaptureFlags) -> Result<Handle> {
    prepare(source, &AtomicBool::new(false))?.start(flags)
}
