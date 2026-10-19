//! The window: TV list (mDNS + manual IP), cast buttons, live status, compact settings.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use nanocast_core::discovery::{self, Browser, ReceiverInfo};
use nanocast_win::capture::Source;
use nanocast_win::mf_encoder;
use nanocast_win::session::{CastConfig, CastSession, CastStats, EndReason, Phase};
use nanocast_win::sources::{self, MonitorInfo, WindowInfo};

const ICON_PNG: &[u8] = include_bytes!("../../../assets/generated/icon-64.png");
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// Packet pacing ceiling (PLAN.md section 4).
const PACE_MBPS: u64 = 80;

pub fn run() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("NANOCast")
        .with_inner_size([480.0, 400.0])
        .with_min_inner_size([420.0, 320.0]);
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
        format!("{}:{}", self.info.addr, self.info.port)
    }
}

struct Settings {
    /// `None`: first working hardware encoder in merit order.
    encoder: Option<String>,
    bitrate_mbps: u32,
    fps: u32,
}

struct ActiveCast {
    session: CastSession,
    tv_name: String,
    source_label: String,
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

    encoders: Result<Vec<String>, String>,
    settings: Settings,

    cast: Option<ActiveCast>,
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
        Self {
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
            encoders: mf_encoder::hw_encoder_names().map_err(|e| format!("{e:#}")),
            settings: Settings { encoder: None, bitrate_mbps: 20, fps: 60 },
            cast: None,
            message: None,
        }
    }

    fn tvs(&self) -> Vec<Tv> {
        merge_tvs(&self.found, &self.manual)
    }

    fn refresh(&mut self) {
        // Every 3 s, faster while nothing is found yet so the first TV shows up within ~0.5 s.
        let every = if self.found.is_empty() { Duration::from_millis(500) } else { REFRESH_EVERY };
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
        let selected_gone = self.selected.as_ref().is_some_and(|k| !tvs.iter().any(|t| &t.key() == k));
        if selected_gone || (self.selected.is_none() && tvs.len() == 1) {
            self.selected = (tvs.len() == 1).then(|| tvs[0].key());
        }
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
            Err(_) => self.manual_error = Some("IP không hợp lệ (vd. 192.168.1.20)".into()),
        }
    }

    fn selected_tv(&self) -> Option<Tv> {
        let key = self.selected.as_ref()?;
        self.tvs().into_iter().find(|t| &t.key() == key)
    }

    fn start_cast(&mut self, source: Source, source_label: String) {
        let Some(tv) = self.selected_tv() else {
            self.message = Some(("Chọn một TV trước.".into(), true));
            return;
        };
        self.stop_cast();
        let cfg = CastConfig {
            video_addr: SocketAddr::new(tv.info.addr.into(), tv.info.port),
            ctrl_port: tv.info.ctrl_port,
            source,
            fps: self.settings.fps,
            bitrate_mbps: self.settings.bitrate_mbps,
            encoder: self.settings.encoder.clone(),
            pace_mbps: PACE_MBPS,
            sender_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "NANOCast PC".into()),
        };
        log::info!("cast {source_label} -> {} ({})", tv.info.name, cfg.video_addr);
        self.message = None;
        self.cast = Some(ActiveCast { session: CastSession::start(cfg), tv_name: tv.info.name, source_label });
    }

    fn stop_cast(&mut self) {
        if let Some(mut c) = self.cast.take() {
            c.session.stop();
        }
    }

    fn casting(&self) -> bool {
        self.cast.as_ref().is_some_and(|c| c.session.is_active())
    }

    // ---------- UI ----------

    fn ui_tvs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("TV trong mạng");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("↻ Quét lại").on_hover_text("Tìm lại TV trong mạng (mDNS)").clicked() {
                    self.rescan();
                }
            });
        });
        let tvs = self.tvs();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_height(56.0);
            ui.set_width(ui.available_width());
            if tvs.is_empty() {
                ui.weak("Đang tìm TV… Mở app NANOCast trên TV (cùng mạng Wi-Fi/LAN).");
                if let Some(e) = &self.browser_error {
                    ui.colored_label(Color32::from_rgb(200, 80, 60), format!("mDNS lỗi: {e}. Hãy nhập IP của TV."));
                }
            }
            egui::ScrollArea::vertical().max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
                for tv in &tvs {
                    let key = tv.key();
                    let text = format!("📺 {}", tv.info.name);
                    let detail = match (tv.manual, tv.info.width.zip(tv.info.height)) {
                        (true, _) => "nhập tay".to_string(),
                        (false, Some((w, h))) => format!("{} · {w}×{h}", tv.info.addr),
                        (false, None) => tv.info.addr.to_string(),
                    };
                    let selected = self.selected.as_deref() == Some(key.as_str());
                    ui.horizontal(|ui| {
                        if ui.selectable_label(selected, text).clicked() {
                            self.selected = Some(key.clone());
                        }
                        ui.weak(detail);
                    });
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label("IP của TV:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.manual_ip).hint_text("192.168.1.20").desired_width(130.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Thêm").clicked() || enter {
                self.add_manual();
            }
            if let Some(e) = &self.manual_error {
                ui.colored_label(Color32::from_rgb(200, 80, 60), e);
            }
        });
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        let have_tv = self.selected_tv().is_some();
        let casting = self.casting();
        ui.horizontal(|ui| {
            let cast_screen = ui
                .add_enabled(have_tv, egui::Button::new(RichText::new("🖥 Cast màn hình").strong()))
                .on_disabled_hover_text("Chọn một TV trước");
            if self.monitors.len() > 1 {
                let label = |m: &MonitorInfo| monitor_label(m);
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
                let m = self.monitors.get(self.monitor).cloned();
                let index = m.as_ref().map(|m| m.index).unwrap_or(1);
                let label = m.map(|m| format!("màn hình {}", m.index)).unwrap_or_else(|| "màn hình".into());
                self.start_cast(Source::Monitor(index), label);
            }
            if ui
                .add_enabled(have_tv, egui::Button::new("🗔 Cast ứng dụng…"))
                .on_disabled_hover_text("Chọn một TV trước")
                .clicked()
            {
                self.windows = sources::list_windows();
                self.window_filter.clear();
                self.show_window_picker = true;
            }
            if ui.add_enabled(casting, egui::Button::new("⏹ Dừng")).clicked() {
                self.stop_cast();
                self.message = Some(("Đã dừng cast.".into(), false));
            }
        });
        ui.horizontal(|ui| {
            let mut audio = false;
            ui.add_enabled(false, egui::Checkbox::new(&mut audio, "Âm thanh"));
            ui.weak("(chưa có — Phase 1b)");
        });
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        let Some(cast) = &self.cast else {
            if let Some((msg, err)) = &self.message {
                status_line(ui, msg, *err);
            } else {
                ui.weak("Chọn TV rồi bấm Cast. Bật Game Mode trên TV để giảm trễ.");
            }
            return;
        };
        let s = cast.session.stats();
        match &s.phase {
            Phase::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Đang kết nối tới {}…", cast.tv_name));
                });
            }
            Phase::Casting => {
                ui.label(RichText::new(format!("● Đang cast {} → {}", cast.source_label, cast.tv_name)).color(Color32::from_rgb(40, 160, 90)).strong());
                for line in stats_lines(&s) {
                    ui.monospace(line);
                }
            }
            Phase::Ended(EndReason::Stopped) => status_line(ui, "Đã dừng cast.", false),
            Phase::Ended(EndReason::SourceClosed) => status_line(ui, "Cửa sổ/màn hình đã đóng — đã dừng cast.", false),
            Phase::Failed(e) => status_line(ui, &format!("Lỗi: {e}"), true),
        }
        if let Some((msg, err)) = &self.message {
            status_line(ui, msg, *err);
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Cài đặt").default_open(false).show(ui, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Encoder");
                match &self.encoders {
                    Ok(list) if !list.is_empty() => {
                        let current = self.settings.encoder.clone().unwrap_or_else(|| "Tự động".into());
                        egui::ComboBox::from_id_salt("encoder").width(260.0).selected_text(current).show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.settings.encoder, None, "Tự động (thử lần lượt)");
                            for name in list {
                                ui.selectable_value(&mut self.settings.encoder, Some(name.clone()), name);
                            }
                        });
                    }
                    Ok(_) => {
                        ui.colored_label(Color32::from_rgb(200, 80, 60), "Không có encoder phần cứng H.264");
                    }
                    Err(e) => {
                        ui.colored_label(Color32::from_rgb(200, 80, 60), format!("Lỗi Media Foundation: {e}"));
                    }
                }
                ui.end_row();
                ui.label("Bitrate");
                ui.horizontal(|ui| {
                    for b in [10, 20, 30] {
                        ui.radio_value(&mut self.settings.bitrate_mbps, b, format!("{b} Mbps"));
                    }
                });
                ui.end_row();
                ui.label("Khung hình");
                ui.horizontal(|ui| {
                    for f in [30, 60] {
                        ui.radio_value(&mut self.settings.fps, f, format!("{f} fps"));
                    }
                });
                ui.end_row();
            });
            ui.weak("Áp dụng từ lần Cast tiếp theo.");
        });
    }

    fn ui_window_picker(&mut self, ctx: &egui::Context) {
        if !self.show_window_picker {
            return;
        }
        let mut pick: Option<WindowInfo> = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("window-picker")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading("Chọn ứng dụng để cast");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.window_filter).hint_text("Lọc theo tên…").desired_width(260.0));
                if ui.button("↻ Làm mới").clicked() {
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
                        text.push_str("  (đang thu nhỏ)");
                    }
                    if ui.selectable_label(false, text).clicked() {
                        pick = Some(w.clone());
                    }
                }
                if !any {
                    ui.weak("Không có cửa sổ nào.");
                }
            });
            ui.weak("Menu/hộp thoại bật ra là cửa sổ riêng nên không hiện; cần thấy menu thì dùng Cast màn hình.");
            ui.separator();
            if ui.button("Đóng").clicked() {
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
}

impl eframe::App for NanoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh();
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
        // Two repaints a second: live stats while casting, a fresh TV list otherwise.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stops capture, sends `stop` to the TV and releases the encoder before the process ends.
        self.stop_cast();
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

fn monitor_label(m: &MonitorInfo) -> String {
    let name = if m.name.trim().is_empty() { format!("Màn hình {}", m.index) } else { m.name.clone() };
    format!("{} ({}×{}){}", truncate(&name, 18), m.width, m.height, if m.primary { " ★" } else { "" })
}

/// The numbers shown while casting: PC side, then what the TV reports.
fn stats_lines(s: &CastStats) -> Vec<String> {
    let mut lines = vec![format!(
        "PC  {:>4.0} fps  {:>5.1} Mbps  encode {:>4.1} ms  {}×{}",
        s.fps, s.mbps, s.encode_ms, s.size.0, s.size.1
    )];
    match &s.receiver {
        Some(r) => match r.stats {
            Some(t) if r.connected => lines.push(format!(
                "TV  {:>4.0} fps  mất {} gói (tổng {})  decode {:.1} ms",
                t.fps, t.lost, r.lost_total, t.decode_ms
            )),
            _ if r.connected => lines.push("TV  chưa có số liệu".into()),
            _ => lines.push("TV  mất kênh điều khiển (vẫn gửi hình)".into()),
        },
        None => lines.push("TV  không có kênh điều khiển (chỉ UDP)".into()),
    }
    lines.push(format!("keyframe_request: {}   encoder: {}", s.keyframe_requests, short_encoder(&s.encoder)));
    lines
}

fn short_encoder(name: &str) -> String {
    let n = name.replace("®", "").replace(" H.264 Encoder MFT", "");
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
        ui.colored_label(Color32::from_rgb(200, 80, 60), msg);
    } else {
        ui.label(msg);
    }
}

/// egui's built-in font lacks some Vietnamese glyphs; Segoe UI (on every Windows) has them.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    for (name, file) in [("segoe-ui", "segoeui.ttf"), ("segoe-symbol", "seguisym.ttf")] {
        let path = std::path::Path::new(&windir).join("Fonts").join(file);
        match std::fs::read(&path) {
            Ok(bytes) => {
                fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
                let family = fonts.families.entry(egui::FontFamily::Proportional).or_default();
                // Segoe UI first for text; Segoe UI Symbol after egui's own fonts for icons.
                if name == "segoe-ui" {
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
    use nanocast_win::control_link::{ReceiverReport, TvStats};
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
        let mut s = CastStats { fps: 59.6, mbps: 18.25, encode_ms: 12.4, size: (1920, 1080), keyframe_requests: 2, ..Default::default() };
        s.encoder = "Intel® Quick Sync Video H.264 Encoder MFT".into();
        let lines = stats_lines(&s);
        assert!(lines[0].contains("60 fps") && lines[0].contains("18.2 Mbps") && lines[0].contains("1920×1080"));
        assert!(lines[1].contains("chỉ UDP"));
        assert!(lines[2].contains("keyframe_request: 2") && lines[2].contains("Intel Quick Sync Video"));

        s.receiver = Some(ReceiverReport {
            connected: true,
            stats: Some(TvStats { fps: 59.8, lost: 3, decode_ms: 9.94 }),
            lost_total: 10,
            ..Default::default()
        });
        let lines = stats_lines(&s);
        assert!(lines[1].contains("mất 3 gói (tổng 10)") && lines[1].contains("decode 9.9 ms"), "{}", lines[1]);
    }

    #[test]
    fn truncates_by_chars() {
        assert_eq!(truncate("Trình chiếu", 20), "Trình chiếu");
        assert_eq!(truncate("Trình chiếu PowerPoint", 6), "Trình…");
    }
}
