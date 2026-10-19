//! What can be cast: displays and application windows (ScreenCaptureKit shareable
//! content), plus the Screen Recording permission (TCC) checks.
//!
//! The types match `nanocast_win::sources` so the GUI code is shared; `WindowInfo::hwnd`
//! holds the CGWindowID on macOS.

use std::sync::mpsc::sync_channel;
use std::time::Duration;

use anyhow::{anyhow, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{NSError, NSOperatingSystemVersion, NSProcessInfo};
use objc2_screen_capture_kit::{SCDisplay, SCShareableContent, SCWindow};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// One-based index into the ScreenCaptureKit display list (`capture::Source::Monitor`).
    pub index: usize,
    pub name: String,
    /// Size in pixels (Retina: backing pixels, e.g. 2880x1800).
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// CGWindowID (named `hwnd` to keep the Windows field name; stable while the window lives).
    pub hwnd: isize,
    pub title: String,
    /// Owning application name.
    pub process: String,
    /// Not on screen right now (minimized, hidden app or another Space).
    pub minimized: bool,
}

/// ScreenCaptureKit's `SCShareableContent` is immutable once delivered; it is only moved
/// from the completion-handler queue to the waiting thread.
struct SendContent(Retained<SCShareableContent>);
unsafe impl Send for SendContent {}

/// True on macOS `major.minor` or later.
pub fn os_at_least(major: isize, minor: isize) -> bool {
    let v = NSOperatingSystemVersion { majorVersion: major, minorVersion: minor, patchVersion: 0 };
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(v)
}

/// ScreenCaptureKit system audio (`SCStreamConfiguration.capturesAudio`) needs macOS 13.
pub fn audio_supported() -> bool {
    os_at_least(13, 0)
}

/// Whether this app already has the Screen Recording permission (never shows a prompt).
pub fn has_screen_recording_permission() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Asks for the Screen Recording permission. The first time this shows the system
/// prompt; afterwards it only returns the current state (the user has to flip the switch
/// in System Settings > Privacy & Security > Screen Recording and restart the app).
pub fn request_screen_recording_permission() -> bool {
    CGRequestScreenCaptureAccess()
}

/// Fetches displays and windows (blocks up to 5 s). The first call triggers the Screen
/// Recording prompt when the permission is not decided yet; without it this fails.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    if !os_at_least(12, 3) {
        return Err(anyhow!("ScreenCaptureKit needs macOS 12.3 or later"));
    }
    // Touching CoreGraphics first sets up the window-server connection; command-line
    // processes without NSApplication otherwise trip CGS_REQUIRE_INIT inside SCK.
    let _ = CGMainDisplayID();
    let (tx, rx) = sync_channel::<Result<SendContent, String>>(1);
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendContent(c)),
            None => Err(unsafe { error.as_ref() }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "no shareable content".into())),
        };
        let _ = tx.try_send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &handler,
        )
    };
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(anyhow!(
            "ScreenCaptureKit: {e} (grant Screen Recording in System Settings > Privacy & Security, then restart NANOCast)"
        )),
        Err(_) => Err(anyhow!("ScreenCaptureKit did not answer within 5 s")),
    }
}

/// Display size in pixels (points x backing scale).
pub(crate) fn display_pixels(d: &SCDisplay) -> (u32, u32) {
    let id = unsafe { d.displayID() };
    let mode = CGDisplayCopyDisplayMode(id);
    let (w, h) = (CGDisplayMode::pixel_width(mode.as_deref()), CGDisplayMode::pixel_height(mode.as_deref()));
    if w > 0 && h > 0 {
        (w as u32, h as u32)
    } else {
        unsafe { (d.width() as u32, d.height() as u32) }
    }
}

pub(crate) fn displays(content: &SCShareableContent) -> Vec<Retained<SCDisplay>> {
    unsafe { content.displays() }.to_vec()
}

pub(crate) fn find_window(content: &SCShareableContent, id: u32) -> Option<Retained<SCWindow>> {
    unsafe { content.windows() }.to_vec().into_iter().find(|w| unsafe { w.windowID() } == id)
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    let content = match shareable_content() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("list displays: {e:#}");
            return Vec::new();
        }
    };
    let main = CGMainDisplayID();
    displays(&content)
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (width, height) = display_pixels(d);
            let id = unsafe { d.displayID() };
            MonitorInfo { index: i + 1, name: format!("Màn hình {}", i + 1), width, height, primary: id == main }
        })
        .collect()
}

/// Normal application windows worth casting: layer 0, titled, not tiny, not ours.
pub fn list_windows() -> Vec<WindowInfo> {
    let content = match shareable_content() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("list windows: {e:#}");
            return Vec::new();
        }
    };
    let own_pid = std::process::id() as i32;
    let mut out: Vec<WindowInfo> = unsafe { content.windows() }
        .to_vec()
        .into_iter()
        .filter_map(|w| unsafe {
            if w.windowLayer() != 0 {
                return None;
            }
            let title = w.title()?.to_string().trim().to_string();
            let app = w.owningApplication()?;
            if title.is_empty() || app.processID() == own_pid {
                return None;
            }
            let frame = w.frame();
            if frame.size.width < 80.0 || frame.size.height < 60.0 {
                return None;
            }
            Some(WindowInfo {
                hwnd: w.windowID() as isize,
                title,
                process: app.applicationName().to_string(),
                minimized: !w.isOnScreen(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.process.to_lowercase().cmp(&b.process.to_lowercase()).then(a.title.cmp(&b.title)));
    out
}

/// Whether the window still exists (cheap CoreGraphics query, no SCK round trip).
pub fn window_alive(hwnd: isize) -> bool {
    CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, hwnd as u32).is_some_and(|a| !a.is_empty())
}
