//! nanocast.exe: one small window that finds TVs on the LAN and casts the screen or one
//! application to them. No account, no installer, no admin rights.

// Release builds are a GUI app: no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod adb;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod adb_wifi;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod chromecast;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod config;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod gui;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod i18n;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod tray;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod tv_install;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod widgets;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    gui::run()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("NANOCast sender runs on Windows, macOS and Linux.");
}
