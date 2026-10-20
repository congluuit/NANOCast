//! Windows: an icon in the notification area while the window is closed during a cast
//! (menu: Open / Pause / Stop / Quit), and the global shortcut Ctrl+Shift+P to pause.
//!
//! A hidden winit window gets no frames, so everything here works from the tray / hotkey
//! callbacks themselves: pause goes straight to the cast session, and Open / Stop / Quit
//! show the window again (Win32 `ShowWindow`) and leave the rest to the next frame.
//! Other systems: no tray (closing while casting minimizes), no global shortcut.

use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::gui::LiveCast;
use crate::i18n::Tr;

/// The cast the shortcut and the tray menu act on.
pub type LiveSlot = Arc<Mutex<Option<Arc<LiveCast>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    Stop,
    Quit,
}

#[cfg(windows)]
pub use win::Tray;

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn new(_ctx: &egui::Context, _hwnd: Option<isize>, _live: LiveSlot) -> Self {
        Tray
    }

    pub fn available(&self) -> bool {
        false
    }

    pub fn hotkey(&self) -> bool {
        false
    }

    pub fn show(&mut self, _t: &Tr, _tv: &str) -> bool {
        false
    }

    pub fn hide(&mut self) {}

    pub fn take_actions(&self) -> Vec<Action> {
        Vec::new()
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW};

    const ICON_PNG: &[u8] = include_bytes!("../../../assets/generated/icon-32.png");

    pub struct Tray {
        hwnd: Option<isize>,
        icon: Option<TrayIcon>,
        _hotkeys: Option<GlobalHotKeyManager>,
        actions: Arc<Mutex<Vec<Action>>>,
    }

    fn show_window(hwnd: Option<isize>) {
        if let Some(h) = hwnd {
            let hwnd = HWND(h as *mut std::ffi::c_void);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    impl Tray {
        pub fn new(ctx: &egui::Context, hwnd: Option<isize>, live: LiveSlot) -> Self {
            let actions = Arc::new(Mutex::new(Vec::new()));
            let hotkeys = GlobalHotKeyManager::new()
                .and_then(|m| m.register(HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyP)).map(|_| m));
            let hotkeys = match hotkeys {
                Ok(m) => {
                    let (ctx, live) = (ctx.clone(), live.clone());
                    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
                        if e.state() == HotKeyState::Pressed {
                            if let Some(l) = live.lock().unwrap().as_ref() {
                                l.toggle_pause();
                            }
                            ctx.request_repaint();
                        }
                    }));
                    Some(m)
                }
                Err(e) => {
                    log::warn!("Ctrl+Shift+P shortcut: {e}");
                    None
                }
            };
            {
                let (ctx, acts) = (ctx.clone(), actions.clone());
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    let action = match e.id().as_ref() {
                        "open" => Action::Open,
                        "stop" => Action::Stop,
                        "quit" => Action::Quit,
                        "pause" => {
                            if let Some(l) = live.lock().unwrap().as_ref() {
                                l.toggle_pause();
                            }
                            return;
                        }
                        _ => return,
                    };
                    acts.lock().unwrap().push(action);
                    show_window(hwnd);
                    ctx.request_repaint();
                }));
            }
            {
                let (ctx, acts) = (ctx.clone(), actions.clone());
                TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                    if matches!(e, TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }) {
                        acts.lock().unwrap().push(Action::Open);
                        show_window(hwnd);
                        ctx.request_repaint();
                    }
                }));
            }
            Self { hwnd, icon: None, _hotkeys: hotkeys, actions }
        }

        /// The window can hide behind a tray icon.
        pub fn available(&self) -> bool {
            self.hwnd.is_some()
        }

        /// Ctrl+Shift+P is registered.
        pub fn hotkey(&self) -> bool {
            self._hotkeys.is_some()
        }

        /// Puts the icon in the notification area (menu in the current language); false
        /// when it could not (then the window must not hide).
        pub fn show(&mut self, t: &Tr, tv: &str) -> bool {
            let menu = Menu::new();
            let items = [
                MenuItem::with_id("open", t.tray_open, true, None),
                MenuItem::with_id("pause", t.tray_pause, true, None),
                MenuItem::with_id("stop", t.tray_stop, true, None),
            ];
            for item in &items {
                let _ = menu.append(item);
            }
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&MenuItem::with_id("quit", t.tray_quit, true, None));
            let icon =
                eframe::icon_data::from_png_bytes(ICON_PNG).ok().and_then(|i| tray_icon::Icon::from_rgba(i.rgba, i.width, i.height).ok());
            let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip(crate::i18n::fill(t.tray_tip, &[&tv]));
            if let Some(icon) = icon {
                builder = builder.with_icon(icon);
            }
            match builder.build() {
                Ok(icon) => {
                    self.icon = Some(icon);
                    true
                }
                Err(e) => {
                    log::warn!("tray icon: {e}");
                    false
                }
            }
        }

        pub fn hide(&mut self) {
            self.icon = None;
        }

        pub fn take_actions(&self) -> Vec<Action> {
            std::mem::take(&mut *self.actions.lock().unwrap())
        }
    }
}
