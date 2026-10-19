//! What can be cast: monitors and top-level application windows.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::IsIconic;
use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// One-based index, as `windows_capture::monitor::Monitor::from_index` expects.
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// Raw HWND (stable while the window lives).
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub minimized: bool,
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    let primary = Monitor::primary().ok().map(|m| m.as_raw_hmonitor());
    Monitor::enumerate()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            index: i + 1,
            name: m.name().or_else(|_| m.device_name()).unwrap_or_else(|_| format!("Màn hình {}", i + 1)),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            primary: Some(m.as_raw_hmonitor()) == primary,
        })
        .collect()
}

/// Visible top-level windows worth casting: titled, not cloaked (hidden UWP shells),
/// not tiny, not belonging to this process.
pub fn list_windows() -> Vec<WindowInfo> {
    let own_pid = std::process::id();
    let mut out: Vec<WindowInfo> = Window::enumerate()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            let title = w.title().ok()?.trim().to_string();
            if title.is_empty() || w.process_id().ok()? == own_pid {
                return None;
            }
            let hwnd = HWND(w.as_raw_hwnd());
            if is_cloaked(hwnd) {
                return None;
            }
            let minimized = unsafe { IsIconic(hwnd).as_bool() };
            if !minimized && (w.width().unwrap_or(0) < 80 || w.height().unwrap_or(0) < 60) {
                return None;
            }
            let process = w.process_name().unwrap_or_default();
            Some(WindowInfo { hwnd: hwnd.0 as isize, title, process, minimized })
        })
        .collect();
    out.sort_by(|a, b| a.process.to_lowercase().cmp(&b.process.to_lowercase()).then(a.title.cmp(&b.title)));
    out
}

/// Whether the window still exists and can be captured.
pub fn window_alive(hwnd: isize) -> bool {
    Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void).is_valid()
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let r = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, std::mem::size_of::<u32>() as u32)
    };
    r.is_ok() && cloaked != 0
}
