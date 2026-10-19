//! NANOCast desktop receiver (Phase 2): for an old PC / mini PC / Raspberry Pi plugged into a
//! TV or projector over HDMI. Advertises itself over mDNS, receives the UDP video stream
//! (and audio, when the sender has it on), decodes H.264 and shows it full screen with as
//! little buffering as possible.
//!
//! Threads: `net` (UDP video -> `Assembler` -> `FrameQueue`), `decoder` (`VideoDecoder`
//! -> latest picture), `audio` (UDP Opus -> cpal), control (TCP, see `control`), and the
//! main thread running the winit event loop + wgpu renderer.
//!
//! Usage:
//!   nanocast-receiver [--name "Phong 12"] [--windowed] [--on-top] [--overlay] [--no-audio] [--no-mdns]
//!                     [--port 48322] [--max 1920x1080] [--pin | --pin-code 1234] [--exit-after <sec>]
//! Keys: O = stats overlay on/off, F / F11 = full screen on/off, Esc = quit.
//!
//! `--pin` asks senders for a random 4-digit code shown on screen (`--pin-code` picks it):
//! TXT `pin=1`, `hello` without the code is refused, UDP only from the accepted sender.
//! Pause / black screen from the sender: the last picture stays with a small pause sign,
//! or the screen goes black. H.264 only (no HEVC decoder here; senders fall back).

mod assembler;
mod audio;
mod control;
mod decoder;
mod font;
mod render;
mod stats;

use std::hash::{BuildHasher, Hasher};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nanocast_core::discovery::Advertiser;
use nanocast_core::protocol::{AUDIO_PORT, CONTROL_PORT, VIDEO_PORT};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::assembler::Assembler;
use crate::control::ControlServer;
use crate::decoder::{FrameQueue, LatestPicture, OpenH264Decoder, VideoDecoder};
use crate::render::{Placement, Renderer};
use crate::stats::{RateMeter, Rates, Stats};

/// No picture for this long: show the idle screen (name + IP) again.
const IDLE_AFTER: Duration = Duration::from_secs(2);
/// Kernel receive buffer for the video socket: a 1080p keyframe is ~300 KB.
const RECV_BUFFER: usize = 4 * 1024 * 1024;
/// How often the main loop looks at pause / blank from the control channel.
const FOLLOW_EVERY: Duration = Duration::from_millis(50);

struct Args {
    name: String,
    windowed: bool,
    on_top: bool,
    overlay: bool,
    audio: bool,
    mdns: bool,
    port: u16,
    max_w: u32,
    max_h: u32,
    /// 4-digit code senders must give (`--pin` / `--pin-code`).
    pin: Option<String>,
    exit_after: Option<f64>,
}

/// A 4-digit code from the OS-seeded hasher (no RNG crate needed).
fn random_pin() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    format!("{:04}", h.finish() % 10_000)
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        name: gethostname::gethostname().to_string_lossy().into_owned(),
        windowed: false,
        on_top: false,
        overlay: false,
        audio: true,
        mdns: true,
        port: VIDEO_PORT,
        max_w: 1920,
        max_h: 1080,
        pin: None,
        exit_after: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--name" => a.name = val()?,
            "--windowed" => a.windowed = true,
            "--on-top" => a.on_top = true,
            "--overlay" => a.overlay = true,
            "--no-audio" => a.audio = false,
            "--no-mdns" => a.mdns = false,
            "--port" => a.port = val()?.parse()?,
            "--max" => {
                let v = val()?;
                let (w, h) = v.split_once(['x', 'X']).context("--max WxH")?;
                (a.max_w, a.max_h) = (w.parse()?, h.parse()?);
            }
            "--exit-after" => a.exit_after = Some(val()?.parse()?),
            "--pin" => a.pin = Some(random_pin()),
            "--pin-code" => {
                let v = val()?;
                if v.len() != 4 || !v.bytes().all(|b| b.is_ascii_digit()) {
                    bail!("--pin-code needs 4 digits");
                }
                a.pin = Some(v);
            }
            "-h" | "--help" => {
                println!(
                    "nanocast-receiver [--name NAME] [--windowed] [--on-top] [--overlay] [--no-audio] [--no-mdns] \
                     [--port 48322] [--max 1920x1080] [--pin | --pin-code 1234] [--exit-after SEC]\n\
                     Keys: O overlay, F/F11 full screen, Esc quit"
                );
                std::process::exit(0);
            }
            s => bail!("unknown argument {s} (try --help)"),
        }
    }
    if a.name.trim().is_empty() {
        a.name = "NANOCast Receiver".into();
    }
    Ok(a)
}

fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut v: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && !i.is_link_local())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

fn video_socket(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(e) = s.set_recv_buffer_size(RECV_BUFFER) {
        log::warn!("SO_RCVBUF {RECV_BUFFER}: {e}");
    }
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    s.bind(&addr.into()).with_context(|| format!("binding video UDP {addr} (another receiver running?)"))?;
    log::info!("video listening on UDP {addr} (rcvbuf {} KB)", s.recv_buffer_size().unwrap_or(0) / 1024);
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s)
}

/// Network thread: datagrams -> frames -> decoder queue. With a 4-digit code, datagrams from
/// any address but the accepted sender's are dropped here.
fn run_net(sock: UdpSocket, queue: Arc<FrameQueue>, stats: Arc<Stats>, control: ControlServer) {
    let mut asm = Assembler::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, from)) if control.allows(from.ip()) => n,
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            // Windows reports ICMP port-unreachable of an earlier send as an error on recv.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                log::error!("video socket: {e}");
                return;
            }
        };
        let now = Instant::now();
        if let Some(frame) = asm.on_packet(&buf[..n], now, &stats, || control.request_keyframe("packet_loss")) {
            queue.submit(frame, &stats, |reason| control.request_keyframe(reason));
        }
    }
}

#[derive(Debug)]
enum UserEvent {
    Picture,
}

struct App {
    args: Args,
    ips: Vec<Ipv4Addr>,
    decoder_name: String,
    stats: Arc<Stats>,
    control: ControlServer,
    latest: LatestPicture,
    /// Set by the decoder when it posted a `Picture` event not yet handled (coalesces events).
    picture_pending: Arc<AtomicBool>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    overlay_on: bool,
    idle: bool,
    video_size: Option<(u32, u32)>,
    last_picture: Option<Instant>,
    meter: RateMeter,
    rates: Rates,
    next_tick: Instant,
    started: Instant,
    first_picture: Option<Instant>,
    error: Option<anyhow::Error>,
    /// (paused, blank) last applied from the control channel.
    held: (bool, bool),
}

impl App {
    fn overlay_lines(&self) -> Vec<String> {
        let r = &self.rates;
        let s = &self.stats;
        let session = self.control.session();
        let size = self.video_size.map(|(w, h)| format!("{w}X{h}")).unwrap_or_else(|| "-".into());
        let ctrl = if session.connected {
            let stream = session.stream.map(|(w, h, f)| format!(" {w}X{h}@{f}")).unwrap_or_default();
            format!("CTRL {}{} AUDIO {}", session.sender, stream, if session.audio { "ON" } else { "OFF" })
        } else {
            format!("CTRL TCP {CONTROL_PORT} WAITING (UDP ONLY)")
        };
        let mut lines = Vec::new();
        if session.paused {
            lines.push("‖ TAM DUNG / PAUSED".into());
        }
        if session.blank {
            lines.push("MAN HINH DEN / BLACK SCREEN".into());
        }
        if let Some(codec) = &session.unsupported_codec {
            lines.push(format!("{codec} KHONG HO TRO / NOT SUPPORTED: H.264 ONLY"));
        }
        if let Some(pin) = &self.args.pin {
            lines.push(format!("MA / CODE {pin}  (REFUSED {})", session.rejected));
        }
        lines.extend([
            format!(
                "FPS {:5.1} SHOWN {:5.1} | LOST {:4} | DECODE {:5.2} MS | {:5.1} MBPS",
                r.fps_decoded, r.fps_presented, r.lost, r.decode_ms, r.mbps
            ),
            format!(
                "{} {} | DROP {} KF REQ {} ERR {} | AUDIO PKT {} LOST {} UNDERRUN {}",
                size,
                self.decoder_name,
                s.dropped_frames.load(Relaxed),
                s.keyframe_requests.load(Relaxed),
                s.decoder_errors.load(Relaxed),
                s.audio_packets.load(Relaxed),
                s.audio_lost.load(Relaxed),
                s.audio_underruns.load(Relaxed),
            ),
            ctrl,
            "O OVERLAY  F FULL SCREEN  ESC QUIT".into(),
        ]);
        lines
    }

    fn idle_lines(&self) -> Vec<String> {
        let mut lines = vec!["NANOCAST".into(), String::new(), self.args.name.clone()];
        if self.ips.is_empty() {
            lines.push("NO NETWORK".into());
        }
        lines.extend(self.ips.iter().map(|ip| format!("IP {ip}")));
        if let Some(pin) = &self.args.pin {
            lines.extend([String::new(), format!("MA / CODE  {pin}")]);
        }
        if let Some(codec) = self.control.session().unsupported_codec {
            lines.extend([String::new(), format!("{codec}: KHONG HO TRO / NOT SUPPORTED"), "CHON H.264 TREN MAY TINH".into()]);
        }
        lines.extend([String::new(), "DANG CHO MAY TINH KET NOI...".into(), "WAITING FOR A SENDER".into()]);
        lines
    }

    fn refresh_overlay(&mut self) {
        if self.renderer.is_none() {
            return;
        }
        let size = self.window.as_ref().map(|w| w.inner_size()).unwrap_or(winit::dpi::PhysicalSize::new(1920, 1080));
        // Largest scale up to `want` whose text box still fits the window.
        let fit = |lines: Vec<String>, want: u32, alpha: u8| {
            let mut scale = want.max(1);
            loop {
                let img = font::render(&lines, scale, alpha);
                if scale == 1 || (img.width <= size.width * 19 / 20 && img.height <= size.height * 19 / 20) {
                    return img;
                }
                scale -= 1;
            }
        };
        if self.idle {
            let img = fit(self.idle_lines(), size.height / 216, 160);
            self.renderer.as_mut().unwrap().set_overlay(Some(&img));
        } else if self.overlay_on {
            let img = fit(self.overlay_lines(), size.height / 540, 150);
            self.renderer.as_mut().unwrap().set_overlay(Some(&img));
        } else if self.held.0 && !self.held.1 {
            // Small pause sign on the frozen picture.
            let img = fit(vec!["‖".into()], size.height / 180, 120);
            self.renderer.as_mut().unwrap().set_overlay(Some(&img));
        } else {
            self.renderer.as_mut().unwrap().set_overlay(None);
        }
    }

    fn draw(&mut self) {
        let placement = if self.idle {
            Placement::Center
        } else if !self.overlay_on && self.held.0 && !self.held.1 {
            Placement::TopRight
        } else {
            Placement::TopLeft
        };
        if let Some(r) = self.renderer.as_mut() {
            r.render(placement);
        }
    }

    fn tick(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now < self.next_tick {
            return;
        }
        self.next_tick = now + Duration::from_secs(1);
        self.rates = self.meter.sample(&self.stats);
        let session = self.control.session();
        let since = self.last_picture.map(|t| now - t);
        // Paused / black: no pictures on purpose, so not idle while the sender is there.
        let held = session.connected && (session.paused || session.blank);
        let idle = match since {
            _ if held => false,
            None => true,
            Some(d) => d > IDLE_AFTER || (session.stopped && d > Duration::from_millis(100)),
        };
        if idle && !self.idle {
            log::info!("stream ended, idle screen");
            if let Some(r) = self.renderer.as_mut() {
                r.clear_video();
            }
            self.video_size = None;
        }
        self.idle = idle;
        self.refresh_overlay();
        // Repaint at least once a second (overlay numbers, idle screen).
        self.draw();
        if let Some(limit) = self.args.exit_after {
            if (now - self.started).as_secs_f64() >= limit {
                event_loop.exit();
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title(format!("NANOCast Receiver - {}", self.args.name));
        if self.args.windowed {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        } else {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        if self.args.on_top {
            attrs = attrs.with_window_level(winit::window::WindowLevel::AlwaysOnTop);
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(anyhow::anyhow!("creating window: {e}"));
                event_loop.exit();
                return;
            }
        };
        window.set_cursor_visible(self.args.windowed);
        window.focus_window();
        match Renderer::new(window.clone()) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.refresh_overlay();
        self.draw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: UserEvent) {
        self.picture_pending.store(false, Relaxed);
        let Some(pic) = self.latest.lock().unwrap().take() else { return };
        if self.held.0 || self.held.1 {
            return; // paused (keep the last picture) / black: frames still in flight are not shown
        }
        let Some(r) = self.renderer.as_mut() else { return };
        r.upload(&pic);
        let now = Instant::now();
        self.last_picture = Some(now);
        if self.first_picture.is_none() {
            self.first_picture = Some(now);
            log::info!("first picture {}x{} after {:.2}s", pic.width, pic.height, (now - self.started).as_secs_f64());
        }
        self.video_size = Some((pic.width, pic.height));
        if self.idle {
            self.idle = false;
            self.refresh_overlay();
        }
        self.draw();
        self.stats.on_presented(pic.complete_at.elapsed().as_micros() as u64);
        self.tick(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.refresh_overlay();
                self.draw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, repeat: false, .. }, .. } => {
                match logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character(c) if c.eq_ignore_ascii_case("o") => {
                        self.overlay_on = !self.overlay_on;
                        self.refresh_overlay();
                        self.draw();
                    }
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    Key::Character(c) if c.eq_ignore_ascii_case("f") => self.toggle_fullscreen(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.follow_session();
        self.tick(event_loop);
        let wake = self.next_tick.min(Instant::now() + FOLLOW_EVERY);
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}

impl App {
    /// Applies pause / black screen from the control channel within ~50 ms.
    fn follow_session(&mut self) {
        let s = self.control.session();
        let held = if s.connected { (s.paused, s.blank) } else { (false, false) };
        if held == self.held {
            return;
        }
        log::info!("paused {} blank {}", held.0, held.1);
        if held.1 {
            if let Some(r) = self.renderer.as_mut() {
                r.clear_video();
            }
        }
        if !held.0 && !held.1 {
            // Resumed: count from now so the idle screen does not flash before the next picture.
            self.last_picture = Some(Instant::now());
        }
        self.held = held;
        if held.0 || held.1 {
            self.idle = false;
        }
        self.refresh_overlay();
        self.draw();
    }
    fn toggle_fullscreen(&mut self) {
        if let Some(w) = &self.window {
            let full = w.fullscreen().is_none();
            w.set_fullscreen(full.then_some(Fullscreen::Borderless(None)));
            w.set_cursor_visible(!full);
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,wgpu_core=error,wgpu_hal=error,naga=warn"))
        .init();
    let args = parse_args()?;
    let stats = Arc::new(Stats::new());
    let ips = local_ipv4s();
    log::info!("NANOCast receiver \"{}\" on {:?}", args.name, ips);

    let sock = video_socket(args.port)?;
    if let Some(pin) = &args.pin {
        log::info!("4-digit code {pin}: senders must enter it");
    }
    let control = ControlServer::start(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, CONTROL_PORT)),
        &args.name,
        args.max_w,
        args.max_h,
        args.pin.clone(),
        stats.clone(),
    )?;
    let _advertiser = if args.mdns {
        match Advertiser::new(&args.name, args.port, args.max_w, args.max_h, args.pin.is_some()) {
            Ok(a) => {
                log::info!("advertising _nanocast._udp as \"{}\"", args.name);
                Some(a)
            }
            Err(e) => {
                log::warn!("mDNS advertise failed (senders must type the IP): {e:#}");
                None
            }
        }
    } else {
        None
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();

    let queue = Arc::new(FrameQueue::new());
    let latest: LatestPicture = Default::default();
    let picture_pending = Arc::new(AtomicBool::new(false));
    let dec: Box<dyn VideoDecoder> = Box::new(OpenH264Decoder::new()?);
    let decoder_name = dec.name().to_uppercase();
    {
        let (queue, latest, stats, control, pending) =
            (queue.clone(), latest.clone(), stats.clone(), control.clone(), picture_pending.clone());
        std::thread::Builder::new().name("decoder".into()).spawn(move || {
            decoder::run_decoder(
                dec,
                queue,
                latest,
                stats,
                |reason| control.request_keyframe(reason),
                || {
                    if !pending.swap(true, Relaxed) {
                        let _ = proxy.send_event(UserEvent::Picture);
                    }
                },
            )
        })?;
    }
    {
        let (queue, stats, control) = (queue.clone(), stats.clone(), control.clone());
        std::thread::Builder::new().name("net".into()).spawn(move || run_net(sock, queue, stats, control))?;
    }
    if args.audio {
        let (stats, control) = (stats.clone(), control.clone());
        std::thread::Builder::new().name("audio".into()).spawn(move || {
            if let Err(e) = audio::run(SocketAddr::from((Ipv4Addr::UNSPECIFIED, AUDIO_PORT)), stats, control) {
                log::error!("audio: {e:#}");
            }
        })?;
    }

    let now = Instant::now();
    let mut app = App {
        overlay_on: args.overlay,
        args,
        ips,
        decoder_name,
        meter: RateMeter::new(&stats),
        stats: stats.clone(),
        control,
        latest,
        picture_pending,
        window: None,
        renderer: None,
        idle: true,
        last_picture: None,
        video_size: None,
        rates: Rates::default(),
        next_tick: now,
        started: now,
        first_picture: None,
        error: None,
        held: (false, false),
    };
    event_loop.run_app(&mut app)?;
    queue.close();

    let (n, avg, p50, p95, p99) = stats.session_summary();
    let secs = match (app.first_picture, app.last_picture) {
        (Some(a), Some(b)) => (b - a).as_secs_f64(),
        _ => 0.0,
    };
    println!(
        "decoded {n} frames ({:.1} fps over {secs:.1}s), presented {} (complete -> presented avg {:.2} ms),          complete -> decoded avg {avg:.2} / p50 {p50:.1} / p95 {p95:.1} / p99 {p99:.1} ms (decode call alone {:.2} ms), \
         lost {} packets, dropped {} frames, keyframe requests {}, decoder errors {},          audio {} packets / {} lost / {} underruns",
        if secs > 0.0 { n as f64 / secs } else { 0.0 },
        stats.frames_presented.load(Relaxed),
        stats.present_avg_ms(),
        stats.decode_call_avg_ms(),
        stats.lost_packets.load(Relaxed),
        stats.dropped_frames.load(Relaxed),
        stats.keyframe_requests.load(Relaxed),
        stats.decoder_errors.load(Relaxed),
        stats.audio_packets.load(Relaxed),
        stats.audio_lost.load(Relaxed),
        stats.audio_underruns.load(Relaxed),
    );
    if let Some(r) = &app.renderer {
        println!("renderer: {}", r.adapter_info);
    }
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
