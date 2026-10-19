//! The window: TV list (mDNS + manual IP), cast buttons, pause / black screen, live status,
//! compact settings. Settings and the last TV are kept in `config::Config`; every string
//! comes from `i18n` (Vietnamese / English).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use nanocast_core::adapt::AdaptReason;
use nanocast_core::discovery::{self, Browser, ReceiverInfo};
use nanocast_core::protocol::VideoCodec;

#[cfg(target_os = "linux")]
use nanocast_linux as platform;
#[cfg(target_os = "macos")]
use nanocast_mac as platform;
#[cfg(windows)]
use nanocast_win as platform;
use platform::capture::Source;
use platform::mf_encoder;
use platform::session::{CastConfig, CastSession, CastStats, CodecFallback, EndReason, Phase};
use platform::sources::{self, MonitorInfo, WindowInfo};

use crate::config::{Config, LastTv};
use crate::i18n::{fill, Lang, Tr};

const ICON_PNG: &[u8] = include_bytes!("../../../assets/generated/icon-64.png");
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// Packet pacing ceiling (PLAN.md section 4).
const PACE_MBPS: u64 = 80;
/// How long "cast again when the app opens" waits for the last TV to show up.
const RECAST_WAIT: Duration = Duration::from_secs(20);
const ERROR_RED: Color32 = Color32::from_rgb(200, 80, 60);

pub fn run() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("NANOCast")
        .with_inner_size([500.0, 470.0])
        .with_min_inner_size([440.0, 360.0]);
    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(Arc::new(icon)),
        Err(e) => log::warn!("window icon: {e}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("NANOCast", options, Box::new(|cc| Ok(Box::new(NanoApp::new(&cc.egui_ctx)))))
}

/// A receiver in the list, from mDNS or typed in.
#[derive(Clone, Debug, PartialEq)]
struct Tv {
    info: ReceiverInfo,
    manual: bool,
}

impl Tv {
    fn key(&self) -> String {
        tv_key(&self.info)
    }
}

fn tv_key(info: &ReceiverInfo) -> String {
    format!("{}:{}", info.addr, info.port)
}

struct ActiveCast {
    session: CastSession,
    tv: Tv,
    source: Source,
    source_label: String,
    /// Code sent in `hello` (to tell "wrong code" from "code needed").
    pin: Option<String>,
    /// Saved as the last TV once it is casting.
    remembered: bool,
}

/// "Enter the code on the TV", then cast `source` to `tv`.
struct PinDialog {
    tv: Tv,
    source: Source,
    source_label: String,
    code: String,
    wrong: bool,
}

struct NanoApp {
    browser: Option<Browser>,
    browser_error: Option<String>,
    found: Vec<ReceiverInfo>,
    manual: Vec<ReceiverInfo>,
    last_refresh: Instant,
    selected: Option<String>,
    manual_ip: String,
    manual_error: Option<String>,

    monitors: Vec<MonitorInfo>,
    monitor: usize,
    windows: Vec<WindowInfo>,
    window_filter: String,
    show_window_picker: bool,

    encoders: HashMap<VideoCodec, Result<Vec<String>, String>>,
    /// Settings, last TV, language; written to disk whenever it differs from `saved`.
    config: Config,
    saved: Config,
    system_lang: Lang,
    /// Codes the TVs accepted (or the user typed) this session, by TV key.
    pins: HashMap<String, String>,
    pin_dialog: Option<PinDialog>,
    /// "Cast again when the app opens": waiting for the last TV until then.
    recast_until: Option<Instant>,
    /// macOS: Screen Recording permission (always true elsewhere).
    can_capture: bool,

    cast: Option<ActiveCast>,
    paused: bool,
    blank: bool,
    /// Message and whether it is an error.
    message: Option<(String, bool)>,
}

impl NanoApp {
    fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        let (browser, browser_error) = match Browser::new() {
            Ok(b) => (Some(b), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let monitors = sources::list_monitors();
        let monitor = monitors.iter().position(|m| m.primary).unwrap_or(0);
        let config = Config::load();
        let encoders = [VideoCodec::H264, VideoCodec::Hevc]
            .into_iter()
            .map(|c| (c, mf_encoder::hw_encoder_names(c).map_err(|e| format!("{e:#}"))))
            .collect();
        let mut app = Self {
            browser,
            browser_error,
            found: Vec::new(),
            manual: Vec::new(),
            last_refresh: Instant::now() - REFRESH_EVERY,
            selected: None,
            manual_ip: String::new(),
            manual_error: None,
            monitors,
            monitor,
            windows: Vec::new(),
            window_filter: String::new(),
            show_window_picker: false,
            encoders,
            saved: config.clone(),
            config,
            system_lang: Lang::system(),
            pins: HashMap::new(),
            pin_dialog: None,
            recast_until: None,
            can_capture: screen_permission(),
            cast: None,
            paused: false,
            blank: false,
            message: None,
        };
        if let Some(last) = app.config.last_tv.clone() {
            let info = last_tv_info(&last);
            if let Some(pin) = &last.pin {
                app.pins.insert(tv_key(&info), pin.clone());
            }
            if last.manual {
                app.manual.push(info.clone());
            }
            app.selected = Some(tv_key(&info));
            if app.config.auto_recast {
                app.recast_until = Some(Instant::now() + RECAST_WAIT);
            }
        }
        app
    }

    fn lang(&self) -> Lang {
        self.config.language.unwrap_or(self.system_lang)
    }

    fn t(&self) -> &'static Tr {
        self.lang().tr()
    }

    fn tvs(&self) -> Vec<Tv> {
        merge_tvs(&self.found, &self.manual)
    }

    fn refresh(&mut self) {
        // Every 3 s, faster while nothing is found yet so the first TV shows up within ~0.5 s.
        let every = if self.found.is_empty() || self.recast_until.is_some() { Duration::from_millis(500) } else { REFRESH_EVERY };
        if self.last_refresh.elapsed() < every {
            return;
        }
        self.last_refresh = Instant::now();
        if let Some(b) = &self.browser {
            self.found = b.receivers();
        }
        let monitors = sources::list_monitors();
        if monitors != self.monitors {
            self.monitor = monitors.iter().position(|m| m.primary).unwrap_or(0);
            self.monitors = monitors;
        }
        let tvs = self.tvs();
        // The last TV may come back under a new address (DHCP): follow it by name.
        if let (Some(last), Some(key)) = (&self.config.last_tv, &self.selected) {
            if !tvs.iter().any(|t| &t.key() == key) {
                if let Some(tv) = tvs.iter().find(|t| !t.manual && t.info.name == last.name) {
                    self.selected = Some(tv.key());
                }
            }
        }
        let selected_gone = self.selected.as_ref().is_some_and(|k| !tvs.iter().any(|t| &t.key() == k));
        if (selected_gone && self.recast_until.is_none()) || (self.selected.is_none() && tvs.len() == 1) {
            self.selected = (tvs.len() == 1).then(|| tvs[0].key());
        }
        self.auto_recast(&tvs);
    }

    /// Starts the "cast again" once the last TV is in the list.
    fn auto_recast(&mut self, tvs: &[Tv]) {
        let Some(until) = self.recast_until else { return };
        if self.cast.is_some() || Instant::now() > until {
            self.recast_until = None;
            return;
        }
        let Some(last) = &self.config.last_tv else { return };
        let Some(tv) = tvs
            .iter()
            .find(|t| t.info.addr == last.addr && t.info.port == last.port && (t.manual || t.info.name == last.name))
            .or_else(|| tvs.iter().find(|t| !t.manual && t.info.name == last.name))
            .cloned()
        else {
            return;
        };
        self.recast_until = None;
        self.selected = Some(tv.key());
        log::info!("cast again to {} (auto)", tv.info.name);
        let (source, label) = self.screen_source();
        self.start_cast_to(tv, source, label);
    }

    fn rescan(&mut self) {
        match &self.browser {
            Some(b) => {
                if let Err(e) = b.rescan() {
                    self.browser_error = Some(format!("{e:#}"));
                }
            }
            None => match Browser::new() {
                Ok(b) => {
                    self.browser = Some(b);
                    self.browser_error = None;
                }
                Err(e) => self.browser_error = Some(format!("{e:#}")),
            },
        }
        self.found.clear();
        self.last_refresh = Instant::now() - REFRESH_EVERY;
    }

    fn add_manual(&mut self) {
        match discovery::manual_receiver(&self.manual_ip) {
            Ok(r) => {
                let tv = Tv { info: r.clone(), manual: true };
                if !self.manual.iter().any(|m| m.addr == r.addr && m.port == r.port) {
                    self.manual.push(r);
                }
                self.selected = Some(tv.key());
                self.manual_ip.clear();
                self.manual_error = None;
            }
            Err(_) => self.manual_error = Some(self.t().bad_ip.into()),
        }
    }

    fn selected_tv(&self) -> Option<Tv> {
        let key = self.selected.as_ref()?;
        self.tvs().into_iter().find(|t| &t.key() == key)
    }

    /// The chosen monitor and its label.
    fn screen_source(&self) -> (Source, String) {
        let t = self.t();
        let m = self.monitors.get(self.monitor);
        let index = m.map(|m| m.index).unwrap_or(1);
        let label = m.map(|m| fill(t.screen_n, &[&m.index])).unwrap_or_else(|| t.screen.into());
        (Source::Monitor(index), label)
    }

    fn start_cast(&mut self, source: Source, source_label: String) {
        let Some(tv) = self.selected_tv() else {
            self.message = Some((format!("{}.", self.t().pick_tv_first), true));
            return;
        };
        self.start_cast_to(tv, source, source_label);
    }

    fn start_cast_to(&mut self, tv: Tv, source: Source, source_label: String) {
        self.stop_cast();
        let pin = self.pins.get(&tv.key()).cloned();
        if tv.info.pin && pin.is_none() {
            self.pin_dialog = Some(PinDialog { tv, source, source_label, code: String::new(), wrong: false });
            return;
        }
        let cfg = CastConfig {
            video_addr: SocketAddr::new(tv.info.addr.into(), tv.info.port),
            ctrl_port: tv.info.ctrl_port,
            source: source.clone(),
            fps: self.config.fps,
            bitrate_mbps: self.config.bitrate_mbps,
            encoder: self.config.encoder.clone(),
            codec: self.config.codec,
            adaptive: self.config.adaptive,
            pin: pin.clone(),
            pace_mbps: PACE_MBPS,
            sender_name: sender_name(),
            audio: self.config.audio,
        };
        log::info!("cast {source_label} -> {} ({})", tv.info.name, cfg.video_addr);
        self.message = None;
        self.paused = false;
        self.blank = false;
        self.cast = Some(ActiveCast { session: CastSession::start(cfg), tv, source, source_label, pin, remembered: false });
    }

    fn stop_cast(&mut self) {
        if let Some(mut c) = self.cast.take() {
            c.session.stop();
        }
        self.paused = false;
        self.blank = false;
    }

    fn casting(&self) -> bool {
        self.cast.as_ref().is_some_and(|c| c.session.is_active())
    }

    /// Follows the running cast: remembers the TV once it plays, asks for the code when
    /// the TV refused it.
    fn watch_cast(&mut self) {
        let Some(cast) = self.cast.as_mut() else { return };
        let phase = cast.session.stats().phase;
        match phase {
            Phase::Casting if !cast.remembered => {
                cast.remembered = true;
                if let Some(pin) = &cast.pin {
                    self.pins.insert(cast.tv.key(), pin.clone());
                }
                self.config.last_tv = Some(LastTv {
                    name: cast.tv.info.name.clone(),
                    addr: cast.tv.info.addr,
                    port: cast.tv.info.port,
                    ctrl_port: cast.tv.info.ctrl_port,
                    manual: cast.tv.manual,
                    pin: cast.pin.clone(),
                });
            }
            Phase::Ended(EndReason::PinRejected) => {
                let cast = self.cast.take().unwrap();
                self.pins.remove(&cast.tv.key());
                if let Some(last) = self.config.last_tv.as_mut() {
                    if last.addr == cast.tv.info.addr && last.port == cast.tv.info.port {
                        last.pin = None;
                    }
                }
                self.pin_dialog = Some(PinDialog {
                    tv: cast.tv,
                    source: cast.source,
                    source_label: cast.source_label,
                    code: String::new(),
                    wrong: cast.pin.is_some(),
                });
            }
            _ => {}
        }
    }

    // ---------- UI ----------

    fn ui_tvs(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.horizontal(|ui| {
            ui.strong(t.tv_heading);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t.rescan).on_hover_text(t.rescan_hint).clicked() {
                    self.rescan();
                }
            });
        });
        let tvs = self.tvs();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_height(56.0);
            ui.set_width(ui.available_width());
            if tvs.is_empty() {
                ui.weak(t.searching);
                if let Some(e) = &self.browser_error {
                    ui.colored_label(ERROR_RED, fill(t.mdns_error, &[e]));
                }
            }
            egui::ScrollArea::vertical().max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
                for tv in &tvs {
                    let key = tv.key();
                    let text = format!("📺 {}", tv.info.name);
                    let detail = match (tv.manual, tv.info.width.zip(tv.info.height)) {
                        (true, _) => t.typed_in.to_string(),
                        (false, Some((w, h))) => format!("{} · {w}×{h}", tv.info.addr),
                        (false, None) => tv.info.addr.to_string(),
                    };
                    let selected = self.selected.as_deref() == Some(key.as_str());
                    ui.horizontal(|ui| {
                        if ui.selectable_label(selected, text).clicked() {
                            self.selected = Some(key.clone());
                        }
                        ui.weak(detail);
                        if tv.info.pin {
                            ui.label(t.pin_badge).on_hover_text(t.pin_badge_hint);
                        }
                    });
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(t.tv_ip);
            let r = ui.add(egui::TextEdit::singleline(&mut self.manual_ip).hint_text("192.168.1.20").desired_width(130.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button(t.add).clicked() || enter {
                self.add_manual();
            }
            if let Some(e) = &self.manual_error {
                ui.colored_label(ERROR_RED, e);
            }
        });
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let have_tv = self.selected_tv().is_some() && self.can_capture;
        let casting = self.casting();
        ui.horizontal(|ui| {
            let cast_screen = ui
                .add_enabled(have_tv, egui::Button::new(RichText::new(t.cast_screen).strong()))
                .on_disabled_hover_text(t.pick_tv_first);
            if self.monitors.len() > 1 {
                let label = |m: &MonitorInfo| monitor_label(m, t);
                egui::ComboBox::from_id_salt("monitor")
                    .width(120.0)
                    .selected_text(self.monitors.get(self.monitor).map(label).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for (i, m) in self.monitors.iter().enumerate() {
                            ui.selectable_value(&mut self.monitor, i, label(m));
                        }
                    });
            }
            if cast_screen.clicked() {
                let (source, label) = self.screen_source();
                self.start_cast(source, label);
            }
            if ui.add_enabled(have_tv, egui::Button::new(t.cast_app)).on_disabled_hover_text(t.pick_tv_first).clicked() {
                self.windows = sources::list_windows();
                self.window_filter.clear();
                self.show_window_picker = true;
            }
            if ui.add_enabled(casting, egui::Button::new(t.stop)).clicked() {
                self.stop_cast();
                self.recast_until = None;
                self.message = Some((t.stopped.into(), false));
            }
        });
        if portal_picks_source() && !casting {
            ui.weak(t.portal_hint);
        }
        // Pause / black screen need the control channel (the TV must know).
        let stats = self.cast.as_ref().map(|c| c.session.stats());
        let ctrl = stats.as_ref().is_some_and(|s| s.phase == Phase::Casting && s.receiver.as_ref().is_some_and(|r| r.connected));
        ui.horizontal(|ui| {
            let label = if self.paused { t.resume } else { t.pause };
            let r = ui
                .add_enabled(ctrl, egui::Button::new(label).selected(self.paused))
                .on_hover_text(t.pause_hint)
                .on_disabled_hover_text(if casting { t.no_ctrl_hint } else { t.pause_hint });
            if r.clicked() {
                self.paused = !self.paused;
                if let Some(c) = &self.cast {
                    c.session.set_paused(self.paused);
                }
            }
            let r = ui
                .add_enabled(ctrl, egui::Button::new(t.blank).selected(self.blank))
                .on_hover_text(t.blank_hint)
                .on_disabled_hover_text(if casting { t.no_ctrl_hint } else { t.blank_hint });
            if r.clicked() {
                self.blank = !self.blank;
                if let Some(c) = &self.cast {
                    c.session.set_blank(self.blank);
                }
            }
            ui.separator();
            let audio_ok = audio_supported();
            let r = ui
                .add_enabled(audio_ok, egui::Checkbox::new(&mut self.config.audio, t.audio))
                .on_hover_text(t.audio_hint)
                .on_disabled_hover_text(t.audio_mac13);
            if r.changed() {
                if let Some(c) = &self.cast {
                    c.session.set_audio(self.config.audio);
                }
            }
            if !casting {
                ui.weak(if self.config.audio { t.audio_on_at_start } else { t.audio_off_paren });
            }
        });
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        if !self.can_capture {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ERROR_RED, t.perm_missing);
                if ui.button(t.perm_button).clicked() {
                    request_screen_permission();
                }
            });
        }
        let Some(cast) = &self.cast else {
            if let (Some(_), Some(last)) = (self.recast_until, &self.config.last_tv) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(fill(t.waiting_recast, &[&last.name]));
                });
            } else if let Some((msg, err)) = &self.message {
                status_line(ui, msg, *err);
            } else {
                ui.weak(t.idle_hint);
            }
            return;
        };
        let s = cast.session.stats();
        match &s.phase {
            Phase::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(fill(t.connecting, &[&cast.tv.info.name]));
                });
            }
            Phase::Casting => {
                ui.label(
                    RichText::new(fill(t.casting, &[&cast.source_label, &cast.tv.info.name]))
                        .color(Color32::from_rgb(40, 160, 90))
                        .strong(),
                );
                if s.blank {
                    ui.label(RichText::new(t.blank_status).strong());
                } else if s.paused {
                    ui.label(RichText::new(t.paused_status).strong());
                }
                for line in stats_lines(&s, t) {
                    ui.monospace(line);
                }
            }
            Phase::Ended(EndReason::Stopped) => status_line(ui, t.stopped, false),
            Phase::Ended(EndReason::SourceClosed) => status_line(ui, t.source_closed, false),
            Phase::Ended(EndReason::PinRejected) => status_line(ui, t.pin_refused, true),
            Phase::Failed(e) => status_line(ui, &fill(t.error, &[e]), true),
        }
        if let Some((msg, err)) = &self.message {
            status_line(ui, msg, *err);
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        egui::CollapsingHeader::new(t.settings).id_salt("settings").default_open(false).show(ui, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(t.codec);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.config.codec, VideoCodec::H264, t.codec_h264);
                    ui.radio_value(&mut self.config.codec, VideoCodec::Hevc, t.codec_hevc).on_hover_text(t.codec_hevc_hint);
                });
                ui.end_row();

                ui.label(t.encoder);
                let codec_name = if self.config.codec == VideoCodec::Hevc { "HEVC" } else { "H.264" };
                match self.encoders.get(&self.config.codec) {
                    Some(Ok(list)) if !list.is_empty() => {
                        let current = self.config.encoder.clone().filter(|e| list.contains(e)).unwrap_or_else(|| t.automatic.into());
                        egui::ComboBox::from_id_salt("encoder").width(260.0).selected_text(current).show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.config.encoder, None, t.automatic_long);
                            for name in list {
                                ui.selectable_value(&mut self.config.encoder, Some(name.clone()), name);
                            }
                        });
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ERROR_RED, fill(t.mf_error, &[e]));
                    }
                    _ => {
                        ui.colored_label(ERROR_RED, fill(t.no_hw_encoder, &[&codec_name]));
                    }
                }
                ui.end_row();

                ui.label(t.bitrate);
                ui.horizontal(|ui| {
                    for b in [10, 20, 30] {
                        ui.radio_value(&mut self.config.bitrate_mbps, b, format!("{b} Mbps"));
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.config.adaptive, t.adaptive).on_hover_text(t.adaptive_hint);
                ui.end_row();

                ui.label(t.frame_rate);
                ui.horizontal(|ui| {
                    for f in [30, 60] {
                        ui.radio_value(&mut self.config.fps, f, format!("{f} fps"));
                    }
                });
                ui.end_row();

                ui.label("");
                ui.checkbox(&mut self.config.auto_recast, t.auto_recast).on_hover_text(t.auto_recast_hint);
                ui.end_row();

                ui.label(t.language);
                let current = self.config.language.map(|l| l.native_name()).unwrap_or(t.lang_system);
                egui::ComboBox::from_id_salt("language").width(160.0).selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.config.language, None, t.lang_system);
                    for l in Lang::ALL {
                        ui.selectable_value(&mut self.config.language, Some(l), l.native_name());
                    }
                });
                ui.end_row();
            });
            ui.weak(t.applies_next);
        });
    }

    fn ui_window_picker(&mut self, ctx: &egui::Context) {
        if !self.show_window_picker {
            return;
        }
        let t = self.t();
        let mut pick: Option<WindowInfo> = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("window-picker")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading(t.pick_app);
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.window_filter).hint_text(t.filter).desired_width(260.0));
                if ui.button(t.refresh).clicked() {
                    self.windows = sources::list_windows();
                }
            });
            let filter = self.window_filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                let mut any = false;
                for w in self.windows.iter().filter(|w| {
                    filter.is_empty() || w.title.to_lowercase().contains(&filter) || w.process.to_lowercase().contains(&filter)
                }) {
                    any = true;
                    let mut text = format!("{} — {}", w.process.trim_end_matches(".exe"), w.title);
                    if w.minimized {
                        text.push_str(t.minimized);
                    }
                    if ui.selectable_label(false, text).clicked() {
                        pick = Some(w.clone());
                    }
                }
                if !any {
                    ui.weak(t.no_windows);
                }
            });
            ui.weak(t.menu_note);
            ui.separator();
            if ui.button(t.close).clicked() {
                close = true;
            }
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(w) = pick {
            self.show_window_picker = false;
            let label = format!("\"{}\"", truncate(&w.title, 28));
            self.start_cast(Source::Window(w.hwnd), label);
        } else if close {
            self.show_window_picker = false;
        }
    }

    fn ui_pin_dialog(&mut self, ctx: &egui::Context) {
        let t = self.t();
        let Some(dialog) = self.pin_dialog.as_mut() else { return };
        let mut submit = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("pin-dialog")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading(t.pin_title);
            ui.label(fill(t.pin_body, &[&dialog.tv.info.name]));
            if dialog.wrong {
                ui.colored_label(ERROR_RED, t.pin_wrong);
            }
            let r = ui.add(
                egui::TextEdit::singleline(&mut dialog.code)
                    .hint_text("0000")
                    .char_limit(4)
                    .desired_width(90.0)
                    .font(egui::TextStyle::Heading),
            );
            dialog.code.retain(|c| c.is_ascii_digit());
            if !r.has_focus() && dialog.code.is_empty() {
                r.request_focus();
            }
            let ready = dialog.code.len() == 4;
            if ready && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(ready, egui::Button::new(RichText::new(t.pin_cast).strong())).clicked() {
                    submit = true;
                }
                if ui.button(t.cancel).clicked() {
                    cancel = true;
                }
            });
        });
        if modal.should_close() {
            cancel = true;
        }
        if submit {
            let d = self.pin_dialog.take().unwrap();
            self.pins.insert(d.tv.key(), d.code.clone());
            self.start_cast_to(d.tv, d.source, d.source_label);
        } else if cancel {
            self.pin_dialog = None;
        }
    }
}

impl eframe::App for NanoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh();
        self.watch_cast();
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            self.ui_tvs(ui);
            ui.separator();
            self.ui_actions(ui);
            ui.separator();
            self.ui_status(ui);
            ui.separator();
            self.ui_settings(ui);
        });
        self.ui_window_picker(&ctx);
        self.ui_pin_dialog(&ctx);
        if self.config != self.saved {
            self.config.save();
            self.saved = self.config.clone();
        }
        // Two repaints a second: live stats while casting, a fresh TV list otherwise.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stops capture, sends `stop` to the TV and releases the encoder before the process ends.
        self.stop_cast();
        if self.config != self.saved {
            self.config.save();
        }
    }
}

/// mDNS results first, then typed-in addresses that mDNS did not already list.
fn merge_tvs(found: &[ReceiverInfo], manual: &[ReceiverInfo]) -> Vec<Tv> {
    let mut tvs: Vec<Tv> = found.iter().map(|r| Tv { info: r.clone(), manual: false }).collect();
    for m in manual {
        if !found.iter().any(|f| f.addr == m.addr && f.port == m.port) {
            tvs.push(Tv { info: m.clone(), manual: true });
        }
    }
    tvs
}

/// A list entry for the saved TV (used when it was typed in, or before mDNS sees it).
fn last_tv_info(last: &LastTv) -> ReceiverInfo {
    ReceiverInfo {
        name: last.name.clone(),
        addr: last.addr,
        port: last.port,
        ctrl_port: last.ctrl_port,
        width: None,
        height: None,
        codec: "h264".into(),
        version: String::new(),
        pin: false,
    }
}

fn monitor_label(m: &MonitorInfo, t: &Tr) -> String {
    let name = if m.name.trim().is_empty() { fill(t.display_n, &[&m.index]) } else { m.name.clone() };
    let star = if m.primary { " ★" } else { "" };
    // 0×0: the size could not be read (Wayland without XWayland).
    if m.width == 0 || m.height == 0 {
        return format!("{}{star}", truncate(&name, 18));
    }
    format!("{} ({}×{}){star}", truncate(&name, 18), m.width, m.height)
}

/// The numbers shown while casting: PC side, what the TV reports, sound, bitrate decision.
fn stats_lines(s: &CastStats, t: &Tr) -> Vec<String> {
    let mut lines = vec![format!(
        "PC  {:>4.0} fps  {:>5.1} Mbps  encode {:>4.1} ms  {}×{}",
        s.fps, s.mbps, s.encode_ms, s.size.0, s.size.1
    )];
    match &s.receiver {
        Some(r) => match r.stats {
            Some(tv) if r.connected => lines.push(format!(
                "TV  {:>4.0} fps  {}  decode {:.1} ms",
                tv.fps,
                fill(t.tv_lost, &[&tv.lost, &r.lost_total]),
                tv.decode_ms
            )),
            _ if r.connected => lines.push(t.tv_no_numbers.into()),
            _ => lines.push(t.tv_ctrl_lost.into()),
        },
        None => lines.push(t.tv_udp_only.into()),
    }
    lines.push(audio_line(s, t));
    lines.push(rate_line(s, t));
    let codec = match s.codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC",
    };
    lines.push(format!("{codec}  keyframe_request: {}   encoder: {}", s.keyframe_requests, short_encoder(&s.encoder)));
    match s.codec_fallback {
        Some(CodecFallback::ReceiverH264Only) => lines.push(t.fallback_receiver.into()),
        Some(CodecFallback::NoEncoder) => lines.push(t.fallback_encoder.into()),
        None => {}
    }
    lines
}

fn audio_line(s: &CastStats, t: &Tr) -> String {
    match (&s.audio_error, s.audio_on) {
        (Some(e), _) => format!("{}  {}", t.sound, fill(t.sound_error, &[&truncate(e, 48)])),
        (None, true) => {
            format!("{}  {:>4.0} kbps Opus{}", t.sound, s.audio_kbps, if s.audio_kbps < 8.0 { format!(" {}", t.silent) } else { String::new() })
        }
        (None, false) => format!("{}  {}", t.sound, t.off),
    }
}

/// Current bitrate / fps and why (adaptive bitrate).
fn rate_line(s: &CastStats, t: &Tr) -> String {
    let reason = if !s.adaptive {
        t.reason_fixed.to_string()
    } else {
        match s.adapt_reason {
            AdaptReason::Start => t.reason_settings.to_string(),
            AdaptReason::PacketLoss { percent } => fill(t.reason_loss, &[&format!("{percent:.1}")]),
            AdaptReason::KeyframeRequests { count } => fill(t.reason_keyframes, &[&count]),
            AdaptReason::SlowDecoder { tv_fps, decode_ms, .. } => {
                fill(t.reason_decoder, &[&format!("{tv_fps:.0}"), &format!("{decode_ms:.0}")])
            }
            AdaptReason::Recovered => t.reason_recovered.to_string(),
        }
    };
    fill(t.rate_line, &[&s.target_mbps, &s.target_fps, &reason])
}

fn short_encoder(name: &str) -> String {
    let n = name.replace("®", "").replace(" H.264 Encoder MFT", "").replace(" H265 Encoder MFT", "").replace(" HEVC Encoder MFT", "");
    truncate(n.trim(), 28)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn status_line(ui: &mut egui::Ui, msg: &str, error: bool) {
    if error {
        ui.colored_label(ERROR_RED, msg);
    } else {
        ui.label(msg);
    }
}

/// Shown on the TV (`hello.name`).
fn sender_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        // GUI apps on Linux often do not get HOSTNAME exported.
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()))
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| if cfg!(target_os = "macos") { "NANOCast Mac".into() } else { "NANOCast PC".into() })
}

#[cfg(windows)]
fn screen_permission() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn screen_permission() -> bool {
    sources::has_screen_recording_permission()
}

#[cfg(target_os = "linux")]
fn screen_permission() -> bool {
    true
}

#[cfg(any(windows, target_os = "linux"))]
fn request_screen_permission() {}

#[cfg(target_os = "macos")]
fn request_screen_permission() {
    sources::request_screen_recording_permission();
}

/// Wayland: the xdg-desktop-portal dialog, not our list, decides which screen/window is shared.
#[cfg(target_os = "linux")]
fn portal_picks_source() -> bool {
    sources::portal_picks_source()
}

#[cfg(any(windows, target_os = "macos"))]
fn portal_picks_source() -> bool {
    false
}

#[cfg(windows)]
fn audio_supported() -> bool {
    true
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn audio_supported() -> bool {
    sources::audio_supported()
}

/// Fonts with every Vietnamese glyph: (name, path, is the main text font).
#[cfg(windows)]
fn system_fonts() -> Vec<(&'static str, std::path::PathBuf, bool)> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let fonts = std::path::Path::new(&windir).join("Fonts");
    vec![("segoe-ui", fonts.join("segoeui.ttf"), true), ("segoe-symbol", fonts.join("seguisym.ttf"), false)]
}

#[cfg(target_os = "macos")]
fn system_fonts() -> Vec<(&'static str, std::path::PathBuf, bool)> {
    vec![
        ("arial", "/System/Library/Fonts/Supplemental/Arial.ttf".into(), true),
        ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf".into(), false),
    ]
}

/// DejaVu Sans / Noto Sans have every Vietnamese glyph; paths differ between Debian and Fedora.
#[cfg(target_os = "linux")]
fn system_fonts() -> Vec<(&'static str, std::path::PathBuf, bool)> {
    let first = |paths: &[&str]| paths.iter().map(std::path::PathBuf::from).find(|p| p.exists());
    let dejavu = first(&[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ]);
    let noto = first(&[
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    ]);
    // The first one found is the main text font; with neither, egui keeps its own font.
    let mut fonts = Vec::new();
    if let Some(p) = dejavu {
        fonts.push(("dejavu-sans", p, true));
    }
    if let Some(p) = noto {
        let main = fonts.is_empty();
        fonts.push(("noto-sans", p, main));
    }
    fonts
}

/// egui's built-in font lacks some Vietnamese glyphs; Segoe UI (every Windows) and Arial
/// (every Mac) have them. A missing file just keeps egui's own font.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path, main) in system_fonts() {
        match std::fs::read(&path) {
            Ok(bytes) => {
                fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
                let family = fonts.families.entry(egui::FontFamily::Proportional).or_default();
                // Main font first for text; the symbol font after egui's own fonts for icons.
                if main {
                    family.insert(0, name.into());
                } else {
                    family.push(name.into());
                }
                fonts.families.entry(egui::FontFamily::Monospace).or_default().push(name.into());
            }
            Err(e) => log::warn!("font {}: {e}", path.display()),
        }
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{EN, VI};
    use platform::control_link::{ReceiverReport, TvStats};
    use std::net::Ipv4Addr;

    fn rx(name: &str, last: u8) -> ReceiverInfo {
        let mut r = discovery::manual_receiver(&format!("192.168.1.{last}")).unwrap();
        r.name = name.into();
        r
    }

    #[test]
    fn manual_entries_do_not_duplicate_discovered_tvs() {
        let found = vec![rx("Phòng 12", 20)];
        let manual = vec![rx("192.168.1.20", 20), rx("192.168.1.30", 30)];
        let tvs = merge_tvs(&found, &manual);
        assert_eq!(tvs.len(), 2);
        assert!(!tvs[0].manual && tvs[0].info.name == "Phòng 12");
        assert!(tvs[1].manual && tvs[1].info.addr == Ipv4Addr::new(192, 168, 1, 30));
        assert_eq!(tvs[1].key(), "192.168.1.30:48322");
    }

    #[test]
    fn stats_text() {
        let mut s = CastStats {
            fps: 59.6,
            mbps: 18.25,
            encode_ms: 12.4,
            size: (1920, 1080),
            keyframe_requests: 2,
            target_mbps: 20,
            target_fps: 60,
            adaptive: true,
            ..Default::default()
        };
        s.encoder = "Intel® Quick Sync Video H.264 Encoder MFT".into();
        let lines = stats_lines(&s, &VI);
        assert!(lines[0].contains("60 fps") && lines[0].contains("18.2 Mbps") && lines[0].contains("1920×1080"));
        assert!(lines[1].contains("chỉ UDP"));
        assert_eq!(lines[2], "Âm thanh  tắt");
        assert_eq!(lines[3], "Bitrate 20 Mbps · 60 fps (theo cài đặt)");
        assert!(lines[4].contains("keyframe_request: 2") && lines[4].contains("Intel Quick Sync Video"), "{}", lines[4]);
        assert_eq!(lines.len(), 5);
        s.audio_on = true;
        s.audio_kbps = 101.4;
        assert_eq!(audio_line(&s, &VI), "Âm thanh   101 kbps Opus");
        s.audio_kbps = 4.0;
        assert!(audio_line(&s, &VI).ends_with("(im lặng)"));
        assert!(audio_line(&s, &EN).ends_with("(silent)"));
        s.audio_error = Some("no default audio output device".into());
        assert!(audio_line(&s, &VI).contains("lỗi: no default"));

        s.receiver = Some(ReceiverReport {
            connected: true,
            stats: Some(TvStats { fps: 59.8, lost: 3, decode_ms: 9.94 }),
            lost_total: 10,
            ..Default::default()
        });
        let lines = stats_lines(&s, &VI);
        assert!(lines[1].contains("mất 3 gói (tổng 10)") && lines[1].contains("decode 9.9 ms"), "{}", lines[1]);
        let lines = stats_lines(&s, &EN);
        assert!(lines[1].contains("lost 3 pkts (total 10)"), "{}", lines[1]);
    }

    #[test]
    fn adaptive_and_codec_lines() {
        let mut s = CastStats { target_mbps: 15, target_fps: 60, adaptive: true, ..Default::default() };
        s.adapt_reason = AdaptReason::PacketLoss { percent: 2.5 };
        assert_eq!(rate_line(&s, &VI), "Bitrate 15 Mbps · 60 fps (giảm vì mất 2.5% gói)");
        assert_eq!(rate_line(&s, &EN), "Bitrate 15 Mbps · 60 fps (lowered: 2.5% packets lost)");
        s.adapt_reason = AdaptReason::KeyframeRequests { count: 3 };
        assert!(rate_line(&s, &VI).contains("keyframe liên tục (3 lần)"));
        s.adaptive = false;
        assert!(rate_line(&s, &EN).ends_with("(fixed)"));
        s.codec_fallback = Some(CodecFallback::ReceiverH264Only);
        assert!(stats_lines(&s, &VI).last().unwrap().contains("TV không hỗ trợ HEVC"));
        s.codec = VideoCodec::Hevc;
        s.codec_fallback = None;
        assert!(stats_lines(&s, &EN).last().unwrap().starts_with("HEVC "));
    }

    #[test]
    fn truncates_by_chars() {
        assert_eq!(truncate("Trình chiếu", 20), "Trình chiếu");
        assert_eq!(truncate("Trình chiếu PowerPoint", 6), "Trình…");
    }
}
