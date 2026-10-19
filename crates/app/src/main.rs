//! nanocast.exe: one small window that finds TVs on the LAN and casts the screen or one
//! application to them. No account, no installer, no admin rights.

// Release builds are a GUI app: no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod gui;

#[cfg(windows)]
fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    gui::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("NANOCast sender currently runs on Windows only (macOS/Linux: PLAN.md Phase 3).");
}
