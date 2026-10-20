//! The window, as three steps for people who just want the picture on the TV:
//! 1. choose a TV (NANOCast receivers by mDNS, Chromecast-built-in TVs, or a typed IP),
//! 2. choose what to show (whole screen / one app) and whether sound goes along,
//! 3. casting: the state in plain words, big Pause / Black screen / Stop buttons, the
//!    numbers under "Details".
//!
//! Around them: a 3-page first-run guide, one Settings page, problems explained with what
//! to do, automatic retry when the connection drops, and (Windows) a tray icon when the
//! window is closed during a cast. Every string comes from `i18n` (Vietnamese / English).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, vec2, RichText};
use nanocast_cast as gcast;
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

use crate::chromecast;
#[cfg(windows)]
use crate::config::ConvertPref;
use crate::config::{Config, LastTv, Quality};
use crate::i18n::{fill, Lang, Tr};
use crate::tray::{self, LiveSlot, Tray};
use crate::widgets::{self as w, Icon, Kind, Tone};

const ICON_PNG: &[u8] = include_bytes!("../../../assets/generated/icon-64.png");
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// Packet pacing ceiling (PLAN.md section 4).
const PACE_MBPS: u64 = 80;
/// How long "cast again when the app opens" waits for the last TV to show up.
const RECAST_WAIT: Duration = Duration::from_secs(20);
/// A dropped connection is tried again after this long...
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// ...this many times in a row before giving up.
const MAX_RETRIES: u32 = 6;
/// With nothing found for this long, step 1 explains what to check.
const NONE_FOUND_AFTER: Duration = Duration::from_secs(8);
/// Chromecast streams are capped here (the TV's own decoder and Wi-Fi, not ours).
const CC_MAX_MBPS: u32 = 10;

pub fn run() -> eframe::Result {
    let mut viewport =
        egui::ViewportBuilder::default().with_title("NANOCast").with_inner_size([560.0, 540.0]).with_min_inner_size([460.0, 460.0]);
    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(Arc::new(icon)),
        Err(e) => log::warn!("window icon: {e}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("NANOCast", options, Box::new(|cc| Ok(Box::new(NanoApp::new(cc)))))
}

/// A TV in step 1.
#[derive(Clone, Debug, PartialEq)]
enum Tv {
    /// The NANOCast app (TV, box or desktop receiver), found by mDNS or typed in.
    Nano { info: ReceiverInfo, manual: bool },
    /// A TV / dongle with Chromecast built in.
    Google(gcast::CastDevice),
}

impl Tv {
    fn key(&self) -> String {
        match self {
            Tv::Nano { info, .. } => tv_key(info),
            Tv::Google(d) => format!("gc:{}:{}", d.addr, d.port),
        }
    }

    fn name(&self) -> &str {
        match self {
            Tv::Nano { info, .. } => &info.name,
            Tv::Google(d) => &d.name,
        }
    }
}

fn tv_key(info: &ReceiverInfo) -> String {
    format!("{}:{}", info.addr, info.port)
}

/// What to show: the platform source, the same for Google Cast, and words for the user.
#[derive(Clone, Debug)]
struct Pick {
    source: Source,
    spec: gcast::SourceSpec,
    label: String,
    /// Pixels, for the Google Cast stream size (0 when unknown).
    size: (u32, u32),
}

/// A NANOCast cast, shared with the tray / Ctrl+Shift+P (they work while the window is
/// hidden and gets no frames).
pub struct LiveCast {
    session: Mutex<CastSession>,
    paused: AtomicBool,
    blank: AtomicBool,
}

impl LiveCast {
    fn new(session: CastSession) -> Self {
        Self { session: Mutex::new(session), paused: AtomicBool::new(false), blank: AtomicBool::new(false) }
    }

    fn stats(&self) -> CastStats {
        self.session.lock().unwrap().stats()
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn blank(&self) -> bool {
        self.blank.load(Ordering::Relaxed)
    }

    fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::Relaxed);
        self.session.lock().unwrap().set_paused(on);
    }

    fn set_blank(&self, on: bool) {
        self.blank.store(on, Ordering::Relaxed);
        self.session.lock().unwrap().set_blank(on);
    }

    /// Ctrl+Shift+P and the tray menu.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn toggle_pause(&self) {
        if controllable(&self.stats()) {
            self.set_paused(!self.paused());
        }
    }

    fn stop(&self) {
        self.session.lock().unwrap().stop();
    }
}

/// The receiver dropped the control channel, or never opened one within 3 s of sending.
fn tv_silent(s: &CastStats, casting_since: Instant) -> bool {
    match &s.receiver {
        Some(r) => !r.connected,
        None => casting_since.elapsed() > Duration::from_secs(3),
    }
}

/// Pause / black screen need the control channel (the TV must know).
fn controllable(s: &CastStats) -> bool {
    s.phase == Phase::Casting && s.receiver.as_ref().is_some_and(|r| r.connected)
}

enum Engine {
    Nano(Arc<LiveCast>),
    Google { pending: Option<chromecast::Pending>, session: Option<gcast::CastSession> },
}

struct ActiveCast {
    engine: Engine,
    tv: Tv,
    pick: Pick,
    /// Code sent in `hello` (to tell "wrong code" from "code needed").
    pin: Option<String>,
    /// Saved as the last TV once it is casting.
    remembered: bool,
    /// The picture reached the TV at least once (wording of "lost" vs "could not connect").
    connected_once: bool,
    /// Retries so far; reset once casting works.
    attempt: u32,
    /// When the session started sending (the TV's first report comes within ~1 s).
    casting_since: Option<Instant>,
    /// Waiting to try again.
    retry_at: Option<Instant>,
    /// Technical text of the last problem (Details only).
    detail: Option<String>,
}

/// How the cast is going, in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Connecting,
    Good,
    Weak,
    Lost,
    Retrying { secs: u64, first: bool },
    Paused,
    Blank,
}

/// "Enter the code on the TV", then cast `pick` to `tv`.
struct PinDialog {
    tv: Tv,
    pick: Pick,
    code: String,
    wrong: bool,
}

/// A message on step 1 / 2: what happened and what to do; technical text under Details.
struct Notice {
    tone: Tone,
    title: String,
    lines: Vec<String>,
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Tv,
    What,
    Casting,
}

struct NanoApp {
    browser: Option<Browser>,
    browser_error: Option<String>,
    found: Vec<ReceiverInfo>,
    manual: Vec<ReceiverInfo>,
    google: chromecast::Scanner,
    google_found: Vec<gcast::CastDevice>,
    last_refresh: Instant,
    /// Since when step 1 has been looking (for "no TV found yet").
    searching_since: Instant,
    manual_ip: String,
    manual_error: Option<String>,
    help_open: bool,
    /// The TV chosen in step 1.
    selected: Option<Tv>,

    monitors: Vec<MonitorInfo>,
    monitor: usize,
    windows: Vec<WindowInfo>,
    window_filter: String,
    /// Step 2 shows the list of apps.
    app_list: bool,

    encoders: HashMap<VideoCodec, Result<Vec<String>, String>>,
    /// Settings, last TV, language; written to disk whenever it differs from `saved`.
    config: Config,
    saved: Config,
    /// Codes the TVs accepted (or the user typed) this session, by TV key.
    pins: HashMap<String, String>,
    pin_dialog: Option<PinDialog>,
    /// "Cast again when the app opens": waiting for the last TV until then.
    recast_until: Option<Instant>,
    /// macOS: Screen Recording permission (always true elsewhere).
    can_capture: bool,

    cast: Option<ActiveCast>,
    notice: Option<Notice>,
    screen: Screen,
    settings_open: bool,
    /// First-run guide page.
    guide: Option<usize>,

    logo: Option<egui::TextureHandle>,
    tray: Tray,
    live: LiveSlot,
    /// The window is hidden behind the tray icon.
    hidden: bool,
    quitting: bool,
}

impl NanoApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        let ctx = &cc.egui_ctx;
        w::install(ctx);
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
        let logo = eframe::icon_data::from_png_bytes(ICON_PNG).ok().map(|i| {
            let image = egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba);
            ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
        });
        let live: LiveSlot = Arc::new(Mutex::new(None));
        let tray = Tray::new(ctx, native_window(cc), live.clone());
        let mut app = Self {
            browser,
            browser_error,
            found: Vec::new(),
            manual: Vec::new(),
            google: chromecast::Scanner::start(),
            google_found: Vec::new(),
            last_refresh: Instant::now() - REFRESH_EVERY,
            searching_since: Instant::now(),
            manual_ip: String::new(),
            manual_error: None,
            help_open: false,
            selected: None,
            monitors,
            monitor,
            windows: Vec::new(),
            window_filter: String::new(),
            app_list: false,
            encoders,
            guide: (!config.onboarded).then_some(0),
            saved: config.clone(),
            config,
            pins: HashMap::new(),
            pin_dialog: None,
            recast_until: None,
            can_capture: screen_permission(),
            cast: None,
            notice: None,
            screen: Screen::Tv,
            settings_open: false,
            logo,
            tray,
            live,
            hidden: false,
            quitting: false,
        };
        if let Some(last) = app.config.last_tv.clone() {
            let info = last_tv_info(&last);
            if let Some(pin) = &last.pin {
                app.pins.insert(tv_key(&info), pin.clone());
            }
            if last.manual {
                app.manual.push(info);
            }
            if app.config.auto_recast {
                app.recast_until = Some(Instant::now() + RECAST_WAIT);
            }
        }
        app
    }

    fn lang(&self) -> Lang {
        // Vietnamese unless English was chosen.
        self.config.language.unwrap_or(Lang::Vi)
    }

    fn t(&self) -> &'static Tr {
        self.lang().tr()
    }

    fn tvs(&self) -> Vec<Tv> {
        merge_tvs(&self.found, &self.manual, &self.google_found)
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
        self.google_found = self.google.devices();
        let monitors = sources::list_monitors();
        if monitors != self.monitors {
            self.monitor = monitors.iter().position(|m| m.primary).unwrap_or(0);
            self.monitors = monitors;
        }
        let tvs = self.tvs();
        // The chosen TV may come back under a new address (DHCP): follow it by name.
        if let Some(Tv::Nano { info, manual: false }) = &self.selected {
            if !tvs.iter().any(|t| t.key() == tv_key(info)) {
                if let Some(tv) = tvs.iter().find(|t| matches!(t, Tv::Nano { manual: false, .. }) && t.name() == info.name) {
                    self.selected = Some(tv.clone());
                }
            }
        }
        self.auto_recast(&tvs);
    }

    /// Starts the "cast again" once the last TV is in the list.
    fn auto_recast(&mut self, tvs: &[Tv]) {
        let Some(until) = self.recast_until else {
            return;
        };
        if self.cast.is_some() || Instant::now() > until {
            self.recast_until = None;
            return;
        }
        let Some(last) = &self.config.last_tv else {
            return;
        };
        let is_last = |t: &&Tv, by_name_only: bool| match t {
            Tv::Nano { info, manual } => {
                if by_name_only {
                    !manual && info.name == last.name
                } else {
                    info.addr == last.addr && info.port == last.port && (*manual || info.name == last.name)
                }
            }
            Tv::Google(_) => false,
        };
        let Some(tv) = tvs.iter().find(|t| is_last(t, false)).or_else(|| tvs.iter().find(|t| is_last(t, true))).cloned() else {
            return;
        };
        self.recast_until = None;
        log::info!("cast again to {} (auto)", tv.name());
        self.selected = Some(tv.clone());
        let pick = self.screen_pick();
        self.start_cast(tv, pick);
    }

    fn add_manual(&mut self) {
        match discovery::manual_receiver(&self.manual_ip) {
            Ok(r) => {
                if !self.manual.iter().any(|m| m.addr == r.addr && m.port == r.port) {
                    self.manual.push(r.clone());
                }
                self.manual_ip.clear();
                self.manual_error = None;
                self.help_open = false;
                self.choose_tv(Tv::Nano { info: r, manual: true });
            }
            Err(_) => self.manual_error = Some(self.t().bad_ip.into()),
        }
    }

    fn choose_tv(&mut self, tv: Tv) {
        self.selected = Some(tv);
        self.notice = None;
        self.app_list = false;
        self.recast_until = None;
        self.screen = Screen::What;
    }

    /// The chosen monitor.
    fn screen_pick(&self) -> Pick {
        let t = self.t();
        let m = self.monitors.get(self.monitor);
        let index = m.map(|m| m.index).unwrap_or(1);
        let label = if self.monitors.len() > 1 { fill(t.display_n, &[&index]) } else { t.whole_screen.to_string() };
        // Google Cast counts monitors from 0 = primary.
        let gc_index = match m {
            Some(m) if m.primary => 0,
            Some(m) => 1 + self.monitors.iter().filter(|o| !o.primary && o.index < m.index).count(),
            None => 0,
        };
        Pick {
            source: Source::Monitor(index),
            spec: gcast::SourceSpec::Monitor(gc_index),
            label,
            size: m.map(|m| (m.width, m.height)).unwrap_or((0, 0)),
        }
    }

    fn window_pick(&self, win: &WindowInfo) -> Pick {
        Pick {
            source: Source::Window(win.hwnd),
            spec: gcast::SourceSpec::Window(win.hwnd as u64),
            label: format!("{} — {}", app_name(&win.process), truncate(&win.title, 32)),
            size: (0, 0),
        }
    }

    fn start_cast(&mut self, tv: Tv, pick: Pick) {
        self.launch(tv, pick, 0);
    }

    fn launch(&mut self, tv: Tv, pick: Pick, attempt: u32) {
        self.stop_cast();
        let mut pin = None;
        let engine = match &tv {
            Tv::Nano { info, .. } => {
                pin = self.pins.get(&tv.key()).cloned();
                if info.pin && pin.is_none() {
                    self.pin_dialog = Some(PinDialog { tv, pick, code: String::new(), wrong: false });
                    return;
                }
                let cfg = CastConfig {
                    video_addr: SocketAddr::new(info.addr.into(), info.port),
                    ctrl_port: info.ctrl_port,
                    source: pick.source.clone(),
                    fps: self.config.fps,
                    bitrate_mbps: self.config.bitrate_mbps,
                    encoder: self.config.encoder.clone(),
                    codec: self.config.codec,
                    adaptive: self.config.adaptive,
                    pin: pin.clone(),
                    pace_mbps: PACE_MBPS,
                    sender_name: sender_name(),
                    audio: self.config.audio,
                    #[cfg(windows)]
                    convert: match self.config.convert {
                        ConvertPref::Auto => platform::session::ConvertPath::Auto,
                        ConvertPref::Gpu => platform::session::ConvertPath::Gpu,
                        ConvertPref::Cpu => platform::session::ConvertPath::Cpu,
                    },
                    #[cfg(windows)]
                    skip_static: self.config.skip_static,
                };
                log::info!("cast {} -> {} ({}), attempt {attempt}", pick.label, info.name, cfg.video_addr);
                let live = Arc::new(LiveCast::new(CastSession::start(cfg)));
                *self.live.lock().unwrap() = Some(live.clone());
                Engine::Nano(live)
            }
            Tv::Google(device) => {
                let (width, height) = if pick.size.0 > 0 && pick.size.1 > 0 { pick.size } else { (1920, 1080) };
                let cfg = gcast::CastConfig {
                    width,
                    height,
                    fps: self.config.fps.min(30),
                    bitrate_bps: self.config.bitrate_mbps.min(CC_MAX_MBPS) * 1_000_000,
                    audio: self.config.audio,
                };
                log::info!("Google Cast {} -> {} ({}), attempt {attempt}", pick.label, device.name, device.addr);
                Engine::Google { pending: Some(chromecast::start(device.clone(), cfg, pick.spec.clone())), session: None }
            }
        };
        self.notice = None;
        self.screen = Screen::Casting;
        self.app_list = false;
        self.cast = Some(ActiveCast {
            engine,
            tv,
            pick,
            pin,
            remembered: false,
            connected_once: false,
            attempt,
            casting_since: None,
            retry_at: None,
            detail: None,
        });
    }

    fn stop_cast(&mut self) {
        *self.live.lock().unwrap() = None;
        if let Some(c) = self.cast.take() {
            match c.engine {
                Engine::Nano(live) => live.stop(),
                Engine::Google { session, .. } => {
                    // A pending start closes its own session (its channel is dropped here).
                    if let Some(s) = session {
                        chromecast::stop(s);
                    }
                }
            }
        }
    }

    /// Stops and goes back to step 2 (or 1) with `notice`.
    fn end_cast(&mut self, notice: Option<Notice>, to_tv_list: bool) {
        self.stop_cast();
        self.notice = notice;
        self.screen = if to_tv_list || self.selected.is_none() { Screen::Tv } else { Screen::What };
        if self.hidden {
            self.show_window_again();
        }
    }

    fn user_stop(&mut self) {
        self.recast_until = None;
        let t = self.t();
        self.end_cast(Some(Notice { tone: Tone::Neutral, title: t.stopped.into(), lines: Vec::new(), detail: None }), false);
    }

    fn casting(&self) -> bool {
        self.cast.is_some()
    }

    /// Follows the running cast: remembers the TV once it plays, asks for the code when the
    /// TV refused it, explains problems and tries again after a dropped connection.
    fn watch_cast(&mut self) {
        let t = self.t();
        let Some(cast) = self.cast.as_mut() else {
            return;
        };
        if let Some(at) = cast.retry_at {
            if Instant::now() >= at {
                self.retry_now();
            }
            return;
        }
        let mut failed: Option<String> = None;
        match &mut cast.engine {
            Engine::Nano(live) => match live.stats().phase {
                Phase::Starting => {}
                // Every NANOCast receiver has a control channel: without one (or once it
                // drops, it does not come back by itself) the TV is not there. The session
                // would keep sending UDP into the void, so start over in 10 s.
                Phase::Casting if tv_silent(&live.stats(), *cast.casting_since.get_or_insert_with(Instant::now)) => {
                    let lost = live.stats().receiver.is_some();
                    if cast.attempt >= MAX_RETRIES {
                        failed = Some(format!("{}: the TV does not answer", tv_name(&cast.tv)));
                    } else {
                        log::info!("TV not answering (lost: {lost}); casting again in {} s", RETRY_AFTER.as_secs());
                        cast.retry_at = Some(Instant::now() + RETRY_AFTER);
                        cast.detail = Some(if lost {
                            "control channel lost: the TV stopped answering".into()
                        } else {
                            "control channel unavailable: the TV does not answer".into()
                        });
                    }
                }
                // Sending, the TV's first report is not in yet.
                Phase::Casting if live.stats().receiver.is_none() => {}
                Phase::Casting => {
                    cast.connected_once = true;
                    cast.attempt = 0;
                    if !cast.remembered {
                        cast.remembered = true;
                        if let Tv::Nano { info, manual } = &cast.tv {
                            if let Some(pin) = &cast.pin {
                                self.pins.insert(cast.tv.key(), pin.clone());
                            }
                            self.config.last_tv = Some(LastTv {
                                name: info.name.clone(),
                                addr: info.addr,
                                port: info.port,
                                ctrl_port: info.ctrl_port,
                                manual: *manual,
                                pin: cast.pin.clone(),
                            });
                        }
                    }
                }
                Phase::Ended(EndReason::Stopped) => self.end_cast(None, false),
                Phase::Ended(EndReason::SourceClosed) => {
                    let n = Notice { tone: Tone::Neutral, title: t.source_closed.into(), lines: Vec::new(), detail: None };
                    self.end_cast(Some(n), false);
                }
                Phase::Ended(EndReason::PinRejected) => {
                    let cast = self.cast.take().unwrap();
                    *self.live.lock().unwrap() = None;
                    self.pins.remove(&cast.tv.key());
                    if let (Some(last), Tv::Nano { info, .. }) = (self.config.last_tv.as_mut(), &cast.tv) {
                        if last.addr == info.addr && last.port == info.port {
                            last.pin = None;
                        }
                    }
                    self.screen = Screen::What;
                    self.pin_dialog = Some(PinDialog { tv: cast.tv, pick: cast.pick, code: String::new(), wrong: cast.pin.is_some() });
                }
                Phase::Failed(e) => failed = Some(e),
            },
            Engine::Google { pending, session } => {
                if let Some(result) = pending.as_ref().and_then(|p| p.poll()) {
                    *pending = None;
                    match result {
                        Ok(s) => *session = Some(s),
                        Err(e) if chromecast::is_unsupported(&e) => {
                            let n = Notice {
                                tone: Tone::Warn,
                                title: t.err_cc_unsupported.into(),
                                lines: vec![t.fix_cc_unsupported_1.into(), t.fix_cc_unsupported_2.into()],
                                detail: Some(e),
                            };
                            self.end_cast(Some(n), true);
                            return;
                        }
                        Err(e) => failed = Some(e),
                    }
                }
                if session.as_ref().is_some_and(|s| s.stats().connected) {
                    cast.connected_once = true;
                    cast.attempt = 0;
                }
            }
        }
        if let Some(e) = failed {
            self.on_failure(e);
        }
    }

    /// A cast failed: try again in 10 s for connection problems, explain the others.
    fn on_failure(&mut self, error: String) {
        let t = self.t();
        let Some(cast) = self.cast.as_mut() else {
            return;
        };
        let (title, fixes, retry) = explain(&error, t);
        let google = matches!(cast.tv, Tv::Google(_));
        if retry && cast.attempt < MAX_RETRIES {
            log::info!("cast failed ({error}); trying again in {} s", RETRY_AFTER.as_secs());
            cast.retry_at = Some(Instant::now() + RETRY_AFTER);
            cast.detail = Some(error);
            *self.live.lock().unwrap() = None;
            return;
        }
        let (title, lines): (&str, Vec<String>) = if google {
            (t.err_cc, vec![t.fix_cc.into()])
        } else if retry {
            (t.err_gave_up, fixes.iter().map(|s| s.to_string()).collect())
        } else {
            (title, fixes.iter().map(|s| s.to_string()).collect())
        };
        self.end_cast(Some(Notice { tone: Tone::Bad, title: title.into(), lines, detail: Some(error) }), false);
    }

    fn retry_now(&mut self) {
        let Some(cast) = self.cast.take() else { return };
        // The old session ended or lost the TV: release it before starting again.
        *self.live.lock().unwrap() = None;
        match cast.engine {
            Engine::Nano(live) => live.stop(),
            Engine::Google { session: Some(s), .. } => chromecast::stop(s),
            Engine::Google { session: None, .. } => {}
        }
        let attempt = cast.attempt + 1;
        self.launch(cast.tv, cast.pick, attempt);
    }

    fn health(&self) -> Option<Health> {
        let cast = self.cast.as_ref()?;
        if let Some(at) = cast.retry_at {
            let secs = at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            return Some(Health::Retrying { secs, first: !cast.connected_once });
        }
        Some(match &cast.engine {
            Engine::Nano(live) => {
                let s = live.stats();
                match s.phase {
                    Phase::Starting => Health::Connecting,
                    Phase::Casting => {
                        if cast.casting_since.is_some_and(|since| tv_silent(&s, since)) {
                            Health::Lost
                        } else if s.receiver.is_none() {
                            Health::Connecting
                        } else if s.blank || live.blank() {
                            Health::Blank
                        } else if s.paused || live.paused() {
                            Health::Paused
                        } else if s.adaptive
                            && matches!(
                                s.adapt_reason,
                                AdaptReason::PacketLoss { .. } | AdaptReason::KeyframeRequests { .. } | AdaptReason::SlowDecoder { .. }
                            )
                        {
                            Health::Weak
                        } else {
                            Health::Good
                        }
                    }
                    _ => Health::Lost,
                }
            }
            Engine::Google { session: Some(s), .. } => {
                if s.stats().connected {
                    Health::Good
                } else if cast.connected_once {
                    Health::Lost
                } else {
                    Health::Connecting
                }
            }
            Engine::Google { session: None, .. } => Health::Connecting,
        })
    }

    fn set_audio(&mut self, on: bool) {
        self.config.audio = on;
        if let Some(c) = self.cast.as_mut() {
            match &mut c.engine {
                Engine::Nano(live) => live.session.lock().unwrap().set_audio(on),
                Engine::Google { session: Some(s), .. } => s.set_audio(on),
                Engine::Google { session: None, .. } => {}
            }
        }
    }

    fn show_window_again(&mut self) {
        self.hidden = false;
        self.tray.hide();
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        for action in self.tray.take_actions() {
            self.show_window_again();
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            match action {
                tray::Action::Open => {}
                tray::Action::Stop => self.user_stop(),
                tray::Action::Quit => {
                    self.stop_cast();
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Closing the window while casting keeps casting: hide behind the tray icon
    /// (Windows) or minimize.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quitting || !self.casting() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        let t = self.t();
        let tv = self.cast.as_ref().map(|c| c.tv.name().to_string()).unwrap_or_default();
        if self.tray.available() && self.tray.show(t, &tv) {
            self.hidden = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    // ---------- UI ----------

    fn ui_header(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(vec2(30.0, 30.0)));
            }
            ui.label(RichText::new("NANOCast").font(w::bold(21.0)).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if w::icon_button(ui, Icon::Gear, t.settings).clicked() {
                    self.settings_open = !self.settings_open;
                    self.guide = None;
                }
                if w::icon_button(ui, Icon::Help, t.guide).clicked() {
                    self.guide = Some(0);
                    self.settings_open = false;
                }
            });
        });
        if self.guide.is_none() && !self.settings_open {
            let step = match self.screen {
                Screen::Tv => 0,
                Screen::What => 1,
                Screen::Casting => 2,
            };
            w::steps(ui, step, [t.step_tv, t.step_what, t.step_cast]);
        }
        ui.add_space(2.0);
        ui.separator();
    }

    fn ui_notice(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let Some(n) = &self.notice else { return };
        let icon = match n.tone {
            Tone::Bad | Tone::Warn => Icon::Warn,
            Tone::Good => Icon::Check,
            _ => Icon::Info,
        };
        let lines: Vec<&str> = n.lines.iter().map(|s| s.as_str()).collect();
        w::banner(ui, n.tone, icon, &n.title, &lines);
        if let Some(d) = &n.detail {
            egui::CollapsingHeader::new(RichText::new(t.details).small()).id_salt("notice-detail").show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(d).monospace()).wrap());
            });
        }
    }

    fn ui_step_tv(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        ui.label(RichText::new(t.tv_title).font(w::bold(24.0)));
        ui.label(RichText::new(t.tv_sub).color(p.weak));
        self.ui_notice(ui);
        if let (Some(_), Some(last)) = (self.recast_until, &self.config.last_tv) {
            let name = last.name.clone();
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(fill(t.waiting_recast, &[&name]));
                if ui.button(t.cancel).clicked() {
                    self.recast_until = None;
                }
            });
        }
        let tvs = self.tvs();
        let none_found = tvs.is_empty() && self.searching_since.elapsed() > NONE_FOUND_AFTER;
        if tvs.is_empty() {
            ui.vertical_centered(|ui| {
                w::radar(ui, if none_found { 80.0 } else { 120.0 });
                ui.label(RichText::new(t.searching).font(w::bold(18.0)));
                ui.label(RichText::new(t.searching_sub).color(p.weak));
            });
            if none_found {
                w::banner(ui, Tone::Warn, Icon::Wifi, t.none_title, &[t.help_1, t.help_2, t.help_3]);
                self.ui_ip_entry(ui);
                ui_get_app(ui, t, true);
            }
        } else {
            let last = self.config.last_tv.as_ref();
            let mut chosen = None;
            for tv in &tvs {
                let (badge, tone, status, locked) = match tv {
                    Tv::Nano { info, manual } => {
                        let is_last = last.is_some_and(|l| l.addr == info.addr && l.port == info.port);
                        let needs_code = info.pin && !self.pins.contains_key(&tv.key());
                        let status = if needs_code {
                            t.st_code.to_string()
                        } else if *manual {
                            t.st_typed.to_string()
                        } else if is_last {
                            t.st_last.to_string()
                        } else {
                            t.st_ready.to_string()
                        };
                        (t.kind_fast, Tone::Good, status, info.pin)
                    }
                    Tv::Google(d) => (t.kind_cc, Tone::Warn, if d.model.is_empty() { "Chromecast".into() } else { d.model.clone() }, false),
                };
                let card = w::TvCard {
                    name: tv.name(),
                    badge,
                    badge_tone: tone,
                    status: &status,
                    locked,
                    chromecast: matches!(tv, Tv::Google(_)),
                };
                if w::tv_card(ui, &card).clicked() {
                    chosen = Some(tv.clone());
                }
            }
            ui.horizontal(|ui| {
                w::radar(ui, 26.0);
                ui.label(RichText::new(t.searching).color(p.weak));
            });
            if let Some(tv) = chosen {
                self.choose_tv(tv);
            }
        }
        if self.browser_error.is_some() {
            w::banner(ui, Tone::Warn, Icon::Warn, t.mdns_off, &[]);
        }
        if none_found {
            // The banner above already says all of it.
            return;
        }
        if !self.help_open {
            ui_get_app(ui, t, false);
        }
        if w::row_button(ui, Icon::Help, t.not_listed, self.help_open).clicked() {
            self.help_open = !self.help_open;
        }
        if self.help_open {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                for line in [t.help_1, t.help_2, t.help_3] {
                    ui.add(egui::Label::new(line).wrap());
                }
            });
            self.ui_ip_entry(ui);
            ui_get_app(ui, t, true);
        }
    }

    fn ui_ip_entry(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.manual_ip)
                    .hint_text(t.ip_hint)
                    .font(w::regular(18.0))
                    .desired_width(220.0)
                    .margin(vec2(10.0, 8.0)),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ok = !self.manual_ip.trim().is_empty();
            if w::big_button(ui, Icon::Check, t.add_tv, None, Kind::Primary, vec2(130.0, 40.0), ok).clicked() || (enter && ok) {
                self.add_manual();
            }
        });
        if let Some(e) = &self.manual_error {
            ui.label(RichText::new(e).color(p.red));
        }
    }

    fn ui_step_what(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        let Some(tv) = self.selected.clone() else {
            self.screen = Screen::Tv;
            return;
        };
        ui.horizontal(|ui| {
            if self.app_list {
                if w::link_button(ui, Icon::Back, t.back).clicked() {
                    self.app_list = false;
                }
            } else if w::link_button(ui, Icon::Back, t.other_tv).clicked() {
                self.screen = Screen::Tv;
                self.notice = None;
            }
            if self.casting() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if w::link_button(ui, Icon::Chevron, t.back_to_cast).clicked() {
                        self.screen = Screen::Casting;
                    }
                });
            }
        });
        if self.app_list {
            self.ui_app_list(ui, &tv);
            return;
        }
        ui.add(egui::Label::new(RichText::new(fill(t.what_title, &[&tv.name()])).font(w::bold(24.0))).wrap());
        self.ui_notice(ui);
        if !self.can_capture {
            w::banner(ui, Tone::Bad, Icon::Warn, t.perm_missing, &[]);
            if ui.button(t.perm_button).clicked() {
                request_screen_permission();
            }
        }
        if self.monitors.len() > 1 {
            ui.horizontal(|ui| {
                ui.label(t.which_screen);
                egui::ComboBox::from_id_salt("monitor")
                    .width(240.0)
                    .selected_text(self.monitors.get(self.monitor).map(|m| monitor_label(m, t)).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for (i, m) in self.monitors.iter().enumerate() {
                            ui.selectable_value(&mut self.monitor, i, monitor_label(m, t));
                        }
                    });
            });
        }
        let gap = ui.spacing().item_spacing.x;
        let tile = vec2(((ui.available_width() - gap) / 2.0).floor(), 150.0);
        let enabled = self.can_capture;
        let (mut screen, mut app) = (false, false);
        ui.horizontal(|ui| {
            screen = w::big_button(ui, Icon::Screen, t.whole_screen, Some(t.whole_screen_sub), Kind::Normal, tile, enabled).clicked();
            app = w::big_button(ui, Icon::App, t.one_app, Some(t.one_app_sub), Kind::Normal, tile, enabled).clicked();
        });
        if portal_picks_source() {
            ui.label(RichText::new(t.portal_hint).color(p.weak));
        }
        if matches!(tv, Tv::Google(_)) {
            ui.label(RichText::new(t.cc_note).color(p.weak));
        }
        ui.add_space(4.0);
        self.ui_sound_row(ui);
        if screen {
            let pick = self.screen_pick();
            self.start_cast(tv, pick);
        } else if app {
            self.windows = list_apps();
            self.window_filter.clear();
            self.app_list = true;
        }
    }

    fn ui_sound_row(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        let audio_ok = audio_supported();
        egui::Frame::new()
            .fill(p.card)
            .stroke(egui::Stroke::new(1.0, p.border))
            .corner_radius(14.0)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let on = self.config.audio && audio_ok;
                    w::icon(ui, if on { Icon::SoundOn } else { Icon::SoundOff }, 28.0, if on { p.green } else { p.weak });
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(t.sound).font(w::bold(17.0)));
                        let sub = if !audio_ok {
                            t.audio_mac13
                        } else if on {
                            t.sound_on
                        } else {
                            t.sound_off
                        };
                        ui.label(RichText::new(sub).color(p.weak));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut v = self.config.audio && audio_ok;
                        if w::switch(ui, &mut v, audio_ok).changed() {
                            self.set_audio(v);
                        }
                    });
                });
            });
    }

    fn ui_app_list(&mut self, ui: &mut egui::Ui, tv: &Tv) {
        let t = self.t();
        let p = w::pal(ui);
        ui.label(RichText::new(t.pick_app).font(w::bold(22.0)));
        ui.horizontal(|ui| {
            w::icon(ui, Icon::Search, 20.0, p.weak);
            ui.add(
                egui::TextEdit::singleline(&mut self.window_filter)
                    .hint_text(t.filter)
                    .desired_width((ui.available_width() - 130.0).max(120.0))
                    .margin(vec2(8.0, 6.0)),
            );
            if ui.button(t.refresh).clicked() {
                self.windows = list_apps();
            }
        });
        let filter = self.window_filter.to_lowercase();
        let mut pick: Option<WindowInfo> = None;
        let mut any = false;
        for win in self.windows.iter().filter(|w| {
            filter.is_empty() || w.title.to_lowercase().contains(&filter) || app_name(&w.process).to_lowercase().contains(&filter)
        }) {
            any = true;
            let note = win.minimized.then_some(t.minimized);
            if w::app_row(ui, &app_name(&win.process), &truncate(&win.title, 60), note).clicked() {
                pick = Some(win.clone());
            }
        }
        if !any {
            ui.label(RichText::new(t.no_windows).color(p.weak));
        }
        ui.add(egui::Label::new(RichText::new(t.menu_note).small().color(p.weak)).wrap());
        if let Some(win) = pick {
            let pick = self.window_pick(&win);
            self.start_cast(tv.clone(), pick);
        }
    }

    fn ui_step_cast(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        let Some(health) = self.health() else {
            self.screen = if self.selected.is_some() { Screen::What } else { Screen::Tv };
            return;
        };
        let cast = self.cast.as_ref().unwrap();
        let tv_name = cast.tv.name().to_string();
        let google = matches!(cast.tv, Tv::Google(_));
        ui.add(egui::Label::new(RichText::new(fill(t.casting_to, &[&tv_name])).font(w::bold(24.0))).wrap());
        ui.label(RichText::new(fill(t.showing, &[&cast.pick.label])).color(p.weak));

        let retry_title;
        let (tone, icon, title, sub) = match health {
            Health::Connecting => (Tone::Accent, Icon::Wifi, t.h_connecting, t.h_connecting_sub),
            Health::Good => (Tone::Good, Icon::Check, t.h_good, t.h_good_sub),
            Health::Weak => (Tone::Warn, Icon::Wifi, t.h_weak, t.h_weak_sub),
            Health::Lost => (Tone::Bad, Icon::Warn, t.h_lost, t.h_lost_sub),
            Health::Retrying { secs, first } => {
                retry_title = fill(if first { t.h_retry_first } else { t.h_retry }, &[&secs]);
                (Tone::Bad, Icon::Refresh, retry_title.as_str(), t.h_retry_sub)
            }
            Health::Paused => (Tone::Warn, Icon::Pause, t.h_paused, t.h_paused_sub),
            Health::Blank => (Tone::Neutral, Icon::Blank, t.h_blank, t.h_blank_sub),
        };
        w::banner(ui, tone, icon, title, &[sub]);
        if matches!(health, Health::Retrying { .. }) && w::link_button(ui, Icon::Refresh, t.retry_now).clicked() {
            self.retry_now();
            return;
        }

        // Pause / Black screen / Stop.
        let (ctrl, paused, blank) = match &cast.engine {
            Engine::Nano(live) => (controllable(&live.stats()), live.paused(), live.blank()),
            Engine::Google { .. } => (false, false, false),
        };
        let gap = ui.spacing().item_spacing.x;
        let size = vec2(((ui.available_width() - 2.0 * gap) / 3.0).floor(), 104.0);
        let disabled_hint = if google { t.cc_no_ctrl } else { t.no_ctrl_hint };
        let (mut pause_clicked, mut blank_clicked, mut stop_clicked) = (false, false, false);
        ui.horizontal(|ui| {
            let (icon, label, kind) = if paused { (Icon::Play, t.resume, Kind::On) } else { (Icon::Pause, t.pause, Kind::Normal) };
            pause_clicked = w::big_button(ui, icon, label, None, kind, size, ctrl).on_disabled_hover_text(disabled_hint).clicked();
            let (icon, label, kind) = if blank { (Icon::Screen, t.unblank, Kind::On) } else { (Icon::Blank, t.blank, Kind::Normal) };
            blank_clicked = w::big_button(ui, icon, label, None, kind, size, ctrl).on_disabled_hover_text(disabled_hint).clicked();
            stop_clicked = w::big_button(ui, Icon::Stop, t.stop, None, Kind::Danger, size, true).clicked();
        });
        if let Engine::Nano(live) = &cast.engine {
            if pause_clicked {
                live.set_paused(!paused);
            }
            if blank_clicked {
                live.set_blank(!blank);
            }
        }
        self.ui_sound_row(ui);
        if w::link_button(ui, Icon::Screen, t.change_what).clicked() {
            self.screen = Screen::What;
            self.app_list = false;
        }
        let mut hints = Vec::new();
        if self.tray.hotkey() && !google {
            hints.push(t.hotkey_hint);
        }
        hints.push(if self.tray.available() { t.tray_hint } else { t.min_hint });
        for h in hints {
            ui.add(egui::Label::new(RichText::new(h).small().color(p.weak)).wrap());
        }
        if let Some(cast) = &self.cast {
            egui::CollapsingHeader::new(t.details).id_salt("details").show(ui, |ui| {
                let lines = match &cast.engine {
                    Engine::Nano(live) => stats_lines(&live.stats(), t),
                    Engine::Google { session: Some(s), .. } => cc_lines(&s.stats(), t),
                    Engine::Google { session: None, .. } => Vec::new(),
                };
                for l in lines {
                    ui.monospace(l);
                }
                if let Some(d) = &cast.detail {
                    ui.add(egui::Label::new(RichText::new(d).monospace().color(p.red)).wrap());
                }
            });
        }
        if stop_clicked {
            self.user_stop();
        }
    }

    fn ui_guide(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        let page = self.guide.unwrap_or(0).min(2);
        w::illustration(ui, page, vec2(ui.available_width(), 190.0));
        ui.add_space(4.0);
        ui.label(RichText::new(t.ob_title[page]).font(w::bold(22.0)));
        ui.add(egui::Label::new(RichText::new(t.ob_body[page]).size(17.0).color(p.text)).wrap());
        if page == 0 {
            ui_get_app(ui, t, false);
        }
        ui.add_space(6.0);
        w::page_dots(ui, 3, page);
        ui.add_space(6.0);
        let mut finish = false;
        ui.horizontal(|ui| {
            if page < 2 && w::link_button(ui, Icon::Chevron, t.ob_skip).clicked() {
                finish = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, icon) = if page < 2 { (t.ob_next, Icon::Chevron) } else { (t.ob_start, Icon::Check) };
                if w::big_button(ui, icon, label, None, Kind::Primary, vec2(160.0, 46.0), true).clicked() {
                    if page < 2 {
                        self.guide = Some(page + 1);
                    } else {
                        finish = true;
                    }
                }
                if page > 0 && w::link_button(ui, Icon::Back, t.back).clicked() {
                    self.guide = Some(page - 1);
                }
            });
        });
        if finish {
            self.guide = None;
            self.config.onboarded = true;
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let p = w::pal(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new(t.settings).font(w::bold(24.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if w::big_button(ui, Icon::Check, t.done, None, Kind::Primary, vec2(110.0, 38.0), true).clicked() {
                    self.settings_open = false;
                }
            });
        });

        ui.label(RichText::new(t.language).font(w::bold(17.0)));
        ui.horizontal(|ui| {
            for lang in [Lang::Vi, Lang::En] {
                let (value, label) = (Some(lang), lang.native_name());
                let selected = self.lang() == lang;
                let text = RichText::new(label).size(16.0).color(if selected { egui::Color32::WHITE } else { p.text });
                if ui.add(egui::Button::new(text).selected(selected).min_size(vec2(0.0, 36.0))).clicked() {
                    self.config.language = value;
                }
            }
        });

        ui.label(RichText::new(t.quality).font(w::bold(17.0)));
        let gap = ui.spacing().item_spacing.x;
        let size = vec2(((ui.available_width() - 2.0 * gap) / 3.0).floor(), 132.0);
        let current = self.config.quality();
        ui.horizontal(|ui| {
            for (q, icon, label, sub) in [
                (Quality::Auto, Icon::Wifi, t.q_auto, t.q_auto_sub),
                (Quality::Sharp, Icon::Search, t.q_sharp, t.q_sharp_sub),
                (Quality::Smooth, Icon::Play, t.q_smooth, t.q_smooth_sub),
            ] {
                let kind = if current == Some(q) { Kind::Primary } else { Kind::Normal };
                if w::big_button(ui, icon, label, Some(sub), kind, size, true).clicked() {
                    self.config.set_quality(q);
                }
            }
        });
        if current.is_none() {
            ui.label(RichText::new(t.q_custom).color(p.weak));
        }

        ui.horizontal(|ui| {
            w::switch(ui, &mut self.config.auto_recast, true);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(t.auto_recast).font(w::bold(16.0)));
                ui.add(egui::Label::new(RichText::new(t.auto_recast_hint).small().color(p.weak)).wrap());
            });
        });
        if w::link_button(ui, Icon::Help, t.show_guide).clicked() {
            self.settings_open = false;
            self.guide = Some(0);
        }

        egui::CollapsingHeader::new(RichText::new(t.advanced).font(w::bold(16.0))).id_salt("advanced").show(ui, |ui| {
            egui::Grid::new("advanced-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
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
                        ui.label(RichText::new(fill(t.mf_error, &[e])).color(p.red));
                    }
                    _ => {
                        ui.label(RichText::new(fill(t.no_hw_encoder, &[&codec_name])).color(p.red));
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

                self.ui_convert_settings(ui, t);
            });
            ui.label(RichText::new(t.applies_next).small().color(p.weak));
        });
    }

    /// Grid rows for the Windows NV12 path and unchanged-frame skipping.
    #[cfg(windows)]
    fn ui_convert_settings(&mut self, ui: &mut egui::Ui, t: &Tr) {
        ui.label(t.convert);
        ui.horizontal(|ui| {
            for (v, label) in [(ConvertPref::Auto, t.convert_auto), (ConvertPref::Gpu, t.convert_gpu), (ConvertPref::Cpu, t.convert_cpu)] {
                ui.radio_value(&mut self.config.convert, v, label).on_hover_text(t.convert_hint);
            }
        });
        ui.end_row();
        ui.label("");
        ui.checkbox(&mut self.config.skip_static, t.skip_static).on_hover_text(t.skip_static_hint);
        ui.end_row();
    }

    #[cfg(not(windows))]
    fn ui_convert_settings(&mut self, _ui: &mut egui::Ui, _t: &Tr) {}

    fn ui_pin_dialog(&mut self, ctx: &egui::Context) {
        let t = self.t();
        let Some(dialog) = self.pin_dialog.as_mut() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("pin-dialog")).show(ctx, |ui| {
            let p = w::pal(ui);
            ui.set_width(360.0);
            ui.horizontal(|ui| {
                w::icon(ui, Icon::Lock, 26.0, p.accent);
                ui.label(RichText::new(t.pin_title).font(w::bold(22.0)));
            });
            ui.add(egui::Label::new(fill(t.pin_body, &[&dialog.tv.name()])).wrap());
            if dialog.wrong {
                ui.label(RichText::new(t.pin_wrong).color(p.red).strong());
            }
            ui.vertical_centered(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut dialog.code)
                        .hint_text(RichText::new("0000").font(w::bold(40.0)).color(p.border))
                        .char_limit(4)
                        .desired_width(170.0)
                        .align(egui::Align2::CENTER_CENTER)
                        .margin(vec2(12.0, 10.0))
                        .font(w::bold(40.0)),
                );
                dialog.code.retain(|c| c.is_ascii_digit());
                if !r.has_focus() && dialog.code.is_empty() {
                    r.request_focus();
                }
                if dialog.code.len() == 4 && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            });
            let ready = dialog.code.len() == 4;
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if w::big_button(ui, Icon::Cast, t.pin_cast, None, Kind::Primary, vec2(170.0, 44.0), ready).clicked() {
                    submit = true;
                }
                if w::big_button(ui, Icon::Back, t.cancel, None, Kind::Normal, vec2(170.0, 44.0), true).clicked() {
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
            self.start_cast(d.tv, d.pick);
        } else if cancel {
            self.pin_dialog = None;
        }
    }
}

impl eframe::App for NanoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_tray(&ctx);
        self.refresh();
        self.watch_cast();
        let frame = egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(egui::Margin::symmetric(20, 12));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            self.ui_header(ui);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.guide.is_some() {
                    self.ui_guide(ui);
                } else if self.settings_open {
                    self.ui_settings(ui);
                } else {
                    match self.screen {
                        Screen::Tv => self.ui_step_tv(ui),
                        Screen::What => self.ui_step_what(ui),
                        Screen::Casting => self.ui_step_cast(ui),
                    }
                }
            });
        });
        self.ui_pin_dialog(&ctx);
        self.handle_close(&ctx);
        if self.config != self.saved {
            self.config.save();
            self.saved = self.config.clone();
        }
        // Live status while casting (countdowns), a fresh TV list otherwise.
        ctx.request_repaint_after(Duration::from_millis(if self.casting() { 250 } else { 500 }));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stops capture, sends `stop` to the TV and releases the encoder before the process ends.
        self.stop_cast();
        self.tray.hide();
        if self.config != self.saved {
            self.config.save();
        }
    }
}

/// The receiver APK of this exact version, attached to its GitHub release by `release.yml`.
fn apk_url() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!("{}/releases/download/v{v}/NANOCast-Receiver-{v}-androidtv.apk", env!("CARGO_PKG_REPOSITORY"))
}

/// "Download the NANOCast TV app (APK)", with how to get it onto the TV when `hint`.
fn ui_get_app(ui: &mut egui::Ui, t: &Tr, hint: bool) {
    if w::link_button(ui, Icon::Download, t.get_app).on_hover_text(apk_url()).clicked() {
        open_in_browser(&apk_url());
    }
    if hint {
        ui.add(egui::Label::new(RichText::new(t.get_app_hint).small().color(w::pal(ui).weak)).wrap());
    }
}

/// Opens `url` in the default browser (eframe is built without its `links` feature).
fn open_in_browser(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        log::warn!("opening {url}: {e}");
    }
}

/// The native window handle (Windows: HWND) for showing the window again from the tray.
#[cfg(windows)]
fn native_window(cc: &eframe::CreationContext) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn native_window(_cc: &eframe::CreationContext) -> Option<isize> {
    None
}

fn tv_name(tv: &Tv) -> String {
    match tv {
        Tv::Nano { info, .. } => format!("{} ({}:{})", info.name, info.addr, info.ctrl_port),
        Tv::Google(d) => format!("{} ({}:{})", d.name, d.addr, d.port),
    }
}

/// NANOCast receivers first (mDNS, then typed in), then Chromecast devices that are not
/// the same box as a NANOCast receiver.
fn merge_tvs(found: &[ReceiverInfo], manual: &[ReceiverInfo], google: &[gcast::CastDevice]) -> Vec<Tv> {
    let mut tvs: Vec<Tv> = found.iter().map(|r| Tv::Nano { info: r.clone(), manual: false }).collect();
    for m in manual {
        if !found.iter().any(|f| f.addr == m.addr && f.port == m.port) {
            tvs.push(Tv::Nano { info: m.clone(), manual: true });
        }
    }
    for d in google.iter().filter(|d| has_screen(d)) {
        let same_box = tvs.iter().any(|t| matches!(t, Tv::Nano { info, .. } if info.addr == d.addr));
        let key = Tv::Google(d.clone()).key();
        if !same_box && !tvs.iter().any(|t| t.key() == key) {
            tvs.push(Tv::Google(d.clone()));
        }
    }
    tvs
}

/// Speakers (Google Home / Nest Mini / Nest Audio, Chromecast Audio, speaker groups)
/// answer Google Cast discovery too, but have nothing to show a picture on.
fn has_screen(d: &gcast::CastDevice) -> bool {
    let model = d.model.to_lowercase();
    !["google home", "nest mini", "home mini", "nest audio", "chromecast audio", "google cast group", "nest wifi"]
        .iter()
        .any(|m| model.contains(m))
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

/// Plain words for a failed cast, the things to try, and whether trying again may help.
fn explain(error: &str, t: &Tr) -> (&'static str, Vec<&'static str>, bool) {
    let e = error.to_lowercase();
    if ["encoder", "mft", "media foundation", "videotoolbox", "vaapi", "openh264"].iter().any(|k| e.contains(k)) {
        (t.err_encoder, vec![t.fix_encoder], false)
    } else if ["capture", "screencapturekit", "portal", "pipewire"].iter().any(|k| e.contains(k)) {
        (t.err_capture, vec![t.fix_capture], false)
    } else {
        (t.err_connect, vec![t.fix_connect_1, t.fix_connect_2, t.fix_connect_3], true)
    }
}

/// Visible app windows without the system's own invisible / helper windows.
fn list_apps() -> Vec<WindowInfo> {
    sources::list_windows().into_iter().filter(|w| !is_system_window(w)).collect()
}

fn is_system_window(w: &WindowInfo) -> bool {
    const PROCESSES: [&str; 13] = [
        "textinputhost",
        "shellexperiencehost",
        "searchhost",
        "searchapp",
        "startmenuexperiencehost",
        "lockapp",
        "systemsettingsbroker",
        "nvcontainer",
        "nvidia share",
        "gamebar",
        "widgets",
        "peopleexperiencehost",
        "dwm",
    ];
    const TITLES: [&str; 5] =
        ["program manager", "windows input experience", "microsoft text input application", "nvidia geforce overlay", "dock"];
    let process = w.process.to_lowercase();
    let process = process.trim_end_matches(".exe");
    PROCESSES.contains(&process) || TITLES.contains(&w.title.trim().to_lowercase().as_str())
}

/// A name people recognize ("POWERPNT.EXE" -> "PowerPoint").
fn app_name(process: &str) -> String {
    let p = process.trim();
    let stem = p.strip_suffix(".exe").or_else(|| p.strip_suffix(".EXE")).unwrap_or(p);
    let known = match stem.to_lowercase().as_str() {
        "powerpnt" => "PowerPoint",
        "winword" => "Word",
        "excel" => "Excel",
        "onenote" => "OneNote",
        "chrome" => "Google Chrome",
        "msedge" => "Microsoft Edge",
        "firefox" => "Firefox",
        "explorer" => "File Explorer",
        "acrord32" | "acrobat" => "Adobe Acrobat",
        "foxitpdfreader" | "foxitreader" => "Foxit Reader",
        "vlc" => "VLC",
        "zoom" => "Zoom",
        "ms-teams" | "teams" => "Microsoft Teams",
        "notepad" => "Notepad",
        "mspaint" => "Paint",
        "code" => "Visual Studio Code",
        "wps" | "wpp" | "et" => "WPS Office",
        "obs64" => "OBS Studio",
        "applicationframehost" => "Windows",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    let mut chars = stem.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => "?".into(),
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

/// The numbers under Details while casting: PC side, what the TV reports, sound, bitrate decision.
fn stats_lines(s: &CastStats, t: &Tr) -> Vec<String> {
    let mut lines =
        vec![format!("PC  {:>4.0} fps  {:>5.1} Mbps  encode {:>4.1} ms  {}×{}", s.fps, s.mbps, s.encode_ms, s.size.0, s.size.1)];
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
    if let Some(line) = convert_line(s, t) {
        lines.push(line);
    }
    match s.codec_fallback {
        Some(CodecFallback::ReceiverH264Only) => lines.push(t.fallback_receiver.into()),
        Some(CodecFallback::NoEncoder) => lines.push(t.fallback_encoder.into()),
        None => {}
    }
    lines
}

/// Where NV12 is made and how many frames were skipped (Windows sender only).
#[cfg(windows)]
fn convert_line(s: &CastStats, t: &Tr) -> Option<String> {
    let path = if s.gpu_convert { t.convert_gpu } else { t.convert_cpu };
    Some(fill(t.convert_stats, &[&path, &s.frames_unchanged]))
}

#[cfg(not(windows))]
fn convert_line(_s: &CastStats, _t: &Tr) -> Option<String> {
    None
}

/// The numbers under Details for a Chromecast.
fn cc_lines(s: &gcast::CastStats, t: &Tr) -> Vec<String> {
    let mode = match s.mode {
        Some(gcast::CastMode::Mirroring) => t.cc_mirroring,
        Some(gcast::CastMode::MediaReceiver) => t.cc_media,
        None => "-",
    };
    let mut lines = vec![fill(t.cc_line, &[&mode, &format!("{:.0}", s.fps), &format!("{:.1}", s.mbps)])];
    if let Some(ms) = s.latency_ms {
        lines.push(fill(t.cc_latency, &[&ms]));
    }
    if !s.message.is_empty() {
        lines.push(s.message.clone());
    }
    lines
}

fn audio_line(s: &CastStats, t: &Tr) -> String {
    match (&s.audio_error, s.audio_on) {
        (Some(e), _) => format!("{}  {}", t.sound, fill(t.sound_error, &[&truncate(e, 48)])),
        (None, true) => {
            format!(
                "{}  {:>4.0} kbps Opus{}",
                t.sound,
                s.audio_kbps,
                if s.audio_kbps < 8.0 { format!(" {}", t.silent) } else { String::new() }
            )
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

    fn cc(name: &str, last: u8) -> gcast::CastDevice {
        gcast::CastDevice {
            name: name.into(),
            model: "Chromecast".into(),
            addr: Ipv4Addr::new(192, 168, 1, last),
            port: 8009,
            id: format!("id{last}"),
        }
    }

    #[test]
    fn merged_list() {
        let found = vec![rx("Phòng 12", 20)];
        let manual = vec![rx("192.168.1.20", 20), rx("192.168.1.30", 30)];
        // .20 runs the NANOCast app and has Chromecast built in: listed once, as NANOCast.
        let mut speaker = cc("Phòng ngủ", 50);
        speaker.model = "Google Nest Mini".into();
        let google = vec![cc("Phòng 12", 20), cc("Phòng 14", 40), speaker];
        let tvs = merge_tvs(&found, &manual, &google);
        assert_eq!(tvs.len(), 3);
        assert!(matches!(&tvs[0], Tv::Nano { info, manual: false } if info.name == "Phòng 12"));
        assert!(matches!(&tvs[1], Tv::Nano { info, manual: true } if info.addr == Ipv4Addr::new(192, 168, 1, 30)));
        assert_eq!(tvs[1].key(), "192.168.1.30:48322");
        assert!(matches!(&tvs[2], Tv::Google(d) if d.name == "Phòng 14"));
        assert_eq!(tvs[2].key(), "gc:192.168.1.40:8009");
    }

    #[test]
    fn plain_word_errors() {
        let (title, fixes, retry) = explain("connecting control channel 192.168.1.20:48323: connection refused", &VI);
        assert_eq!(title, VI.err_connect);
        assert_eq!(fixes.len(), 3);
        assert!(retry);
        let (title, _, retry) = explain("no hardware H.264 encoder (Media Foundation)", &EN);
        assert_eq!(title, EN.err_encoder);
        assert!(!retry);
        let (title, _, retry) = explain("Graphics Capture: item closed", &EN);
        assert_eq!(title, EN.err_capture);
        assert!(!retry);
    }

    #[test]
    fn app_names_and_system_windows() {
        assert_eq!(app_name("POWERPNT.EXE"), "PowerPoint");
        assert_eq!(app_name("chrome.exe"), "Google Chrome");
        assert_eq!(app_name("myapp.exe"), "Myapp");
        assert_eq!(app_name("Keynote"), "Keynote");
        let win = |process: &str, title: &str| WindowInfo { hwnd: 1, title: title.into(), process: process.into(), minimized: false };
        assert!(is_system_window(&win("explorer.exe", "Program Manager")));
        assert!(is_system_window(&win("TextInputHost.exe", "Windows Input Experience")));
        assert!(!is_system_window(&win("explorer.exe", "Documents")));
        assert!(!is_system_window(&win("POWERPNT.EXE", "Bài 3 - PowerPoint")));
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
        if cfg!(windows) {
            assert_eq!(lines[5], "Xử lý hình: CPU · bỏ qua 0 khung không đổi");
            assert_eq!(lines.len(), 6);
        } else {
            assert_eq!(lines.len(), 5);
        }
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
        assert!(stats_lines(&s, &EN).iter().any(|l| l.starts_with("HEVC ")));
    }

    #[cfg(windows)]
    #[test]
    fn convert_line_text() {
        let s = CastStats { gpu_convert: true, frames_unchanged: 412, ..Default::default() };
        assert_eq!(convert_line(&s, &EN).unwrap(), "Image processing: Graphics card · 412 unchanged frames skipped");
    }

    #[test]
    fn chromecast_lines() {
        let s = gcast::CastStats {
            fps: 29.7,
            mbps: 7.94,
            latency_ms: Some(320),
            connected: true,
            mode: Some(gcast::CastMode::Mirroring),
            message: String::new(),
        };
        let lines = cc_lines(&s, &VI);
        assert_eq!(lines[0], "Chromecast (phản chiếu)  30 fps  7.9 Mbps");
        assert_eq!(lines[1], "trễ khoảng 320 ms");
    }

    #[test]
    fn apk_link() {
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            apk_url(),
            format!("https://github.com/congluuit/NANOCast/releases/download/v{v}/NANOCast-Receiver-{v}-androidtv.apk")
        );
    }

    #[test]
    fn truncates_by_chars() {
        assert_eq!(truncate("Trình chiếu", 20), "Trình chiếu");
        assert_eq!(truncate("Trình chiếu PowerPoint", 6), "Trình…");
    }
}
