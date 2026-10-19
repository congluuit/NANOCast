//! What can be cast: monitors and application windows.
//!
//! The types match `nanocast_win::sources` so the GUI code is shared. What the lists mean
//! depends on the capture back end (`capture::backend()`):
//!
//! - X11: RandR monitors and the window manager's client list; `WindowInfo::hwnd` is the
//!   X11 window id and the cast captures exactly that.
//! - Portal (Wayland): an app cannot list other apps' windows, and the system dialog picks
//!   what is shared. Monitors are still listed (through XWayland when present) so the GUI
//!   can show sizes; the window list is one entry, `capture::PORTAL_WINDOW`, that opens the
//!   dialog. `portal_picks_source()` tells the GUI to say so.

use log::warn;

use crate::capture::{self, Backend, PORTAL_WINDOW};
use crate::x11;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// One-based index (`capture::Source::Monitor`).
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// X11 window id, or `capture::PORTAL_WINDOW` (named `hwnd` to keep the Windows field name).
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub minimized: bool,
}

/// System sound can be captured (a PipeWire or PulseAudio back end is compiled in; whether
/// a sound server runs shows up later in `CastStats::audio_error`).
pub fn audio_supported() -> bool {
    cfg!(any(feature = "pipewire", feature = "pulse"))
}

/// True when the xdg-desktop-portal dialog decides which screen/window is shared.
pub fn portal_picks_source() -> bool {
    capture::backend() == Backend::Portal
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    // X11, or XWayland on a Wayland session (sizes are right; the portal still asks).
    let listed = x11::connect().map(|(conn, root)| x11::monitors(&conn, root));
    match listed {
        Ok(mons) if !mons.is_empty() => mons
            .into_iter()
            .enumerate()
            .map(|(i, m)| MonitorInfo {
                index: i + 1,
                name: m.name,
                width: m.width as u32,
                height: m.height as u32,
                primary: m.primary,
            })
            .collect(),
        other => {
            if let Err(e) = other {
                if !portal_picks_source() {
                    warn!("list monitors: {e:#}");
                }
            }
            if portal_picks_source() {
                vec![MonitorInfo {
                    index: 1,
                    name: "Màn hình (chọn trong hộp thoại hệ thống)".into(),
                    width: 0,
                    height: 0,
                    primary: true,
                }]
            } else {
                Vec::new()
            }
        }
    }
}

/// Application windows worth casting: titled, not tiny, not belonging to this process.
pub fn list_windows() -> Vec<WindowInfo> {
    if portal_picks_source() {
        return vec![WindowInfo {
            hwnd: PORTAL_WINDOW,
            title: "Chọn cửa sổ trong hộp thoại hệ thống…".into(),
            process: "xdg-desktop-portal".into(),
            minimized: false,
        }];
    }
    let own_pid = std::process::id();
    let windows = match x11::connect().and_then(|(conn, root)| x11::client_windows(&conn, root)) {
        Ok(w) => w,
        Err(e) => {
            warn!("list windows: {e:#}");
            return Vec::new();
        }
    };
    let mut out: Vec<WindowInfo> = windows
        .into_iter()
        .filter(|w| !w.title.is_empty() && w.pid != Some(own_pid))
        .filter(|w| w.minimized || (w.width >= 80 && w.height >= 60))
        .map(|w| WindowInfo { hwnd: w.id as isize, title: w.title, process: w.process, minimized: w.minimized })
        .collect();
    out.sort_by(|a, b| a.process.to_lowercase().cmp(&b.process.to_lowercase()).then(a.title.cmp(&b.title)));
    out
}

/// Whether the window still exists. The portal pseudo-window is always "alive"; the
/// portal stream itself reports when the shared window goes away.
pub fn window_alive(hwnd: isize) -> bool {
    if hwnd == PORTAL_WINDOW || portal_picks_source() {
        return true;
    }
    if hwnd <= 0 || hwnd > u32::MAX as isize {
        return false;
    }
    x11::connect().map(|(conn, _)| x11::window_exists(&conn, hwnd as u32)).unwrap_or(false)
}
