//! Command-line test of the Linux sender, no GUI needed (run it on the Linux PC):
//!
//!   cargo run -p nanocast-linux --example linux_cast -- --list
//!   cargo run -p nanocast-linux --example linux_cast -- 192.168.1.50                 # monitor 1, 30 s
//!   cargo run -p nanocast-linux --example linux_cast -- 192.168.1.50 window:0x3a00007 60 --audio
//!   cargo run -p nanocast-linux --example linux_cast -- 192.168.1.50 1 30 --encoder=libx264 --fps=30
//!
//! Arguments: receiver IP, source (`1`, `2`... = monitor index, `window:<X11 id>` from
//! `--list`, `window:0` = pick in the portal dialog), seconds, `--audio` to send system
//! sound, `--encoder=<name>`, `--fps=<n>`, `--mbps=<n>`. `NANOCAST_CAPTURE=x11|portal`
//! forces the capture back end, `NANOCAST_PIN` gives the TV's 4-digit code. Prints stats
//! twice a second.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::net::{IpAddr, SocketAddr};
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use nanocast_core::protocol::{CONTROL_PORT, VIDEO_PORT};
    use nanocast_linux::capture::{self, Source};
    use nanocast_linux::session::{CastConfig, CastSession, Phase};
    use nanocast_linux::{mf_encoder, sources};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let audio = args.iter().any(|a| a == "--audio");
    let opt = |name: &str| args.iter().find_map(|a| a.strip_prefix(&format!("--{name}=")).map(str::to_string));
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    println!(
        "capture back end: {:?} (Wayland session: {}), encoders: {:?}",
        capture::backend(),
        capture::is_wayland(),
        mf_encoder::hw_encoder_names(Default::default())?
    );
    if args.iter().any(|a| a == "--list") || pos.is_empty() {
        for m in sources::list_monitors() {
            println!("monitor {}: {} {}x{}{}", m.index, m.name, m.width, m.height, if m.primary { " (primary)" } else { "" });
        }
        for w in sources::list_windows() {
            println!("window:{:#x} [{}] {}{}", w.hwnd, w.process, w.title, if w.minimized { " (minimized)" } else { "" });
        }
        if pos.is_empty() {
            println!("usage: linux_cast <receiver-ip> [1|window:<id>] [seconds] [--audio] [--encoder=NAME] [--fps=N] [--mbps=N]");
        }
        return Ok(());
    }

    let ip: IpAddr = pos[0].parse().context("receiver IP")?;
    let source = match pos.get(1).map(|s| s.as_str()) {
        Some(s) if s.starts_with("window:") => {
            let id = &s[7..];
            let id = match id.strip_prefix("0x") {
                Some(hex) => isize::from_str_radix(hex, 16),
                None => id.parse(),
            };
            Source::Window(id.context("window id")?)
        }
        Some(s) => Source::Monitor(s.parse().context("monitor index")?),
        None => Source::Monitor(1),
    };
    let secs: u64 = pos.get(2).map(|s| s.parse()).transpose().context("seconds")?.unwrap_or(30);
    let fps: u32 = opt("fps").map(|s| s.parse()).transpose().context("--fps")?.unwrap_or(60);
    let mbps: u32 = opt("mbps").map(|s| s.parse()).transpose().context("--mbps")?.unwrap_or(15);

    let mut session = CastSession::start(CastConfig {
        video_addr: SocketAddr::new(ip, VIDEO_PORT),
        ctrl_port: CONTROL_PORT,
        source,
        fps,
        bitrate_mbps: mbps,
        encoder: opt("encoder"),
        codec: Default::default(),
        adaptive: true,
        pin: std::env::var("NANOCAST_PIN").ok(),
        pace_mbps: 80,
        sender_name: "NANOCast linux_cast".into(),
        audio,
    });
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end && session.is_active() {
        std::thread::sleep(Duration::from_millis(500));
        let s = session.stats();
        if s.phase == Phase::Casting {
            println!(
                "{} {}x{}: {:.1} fps {:.2} Mbps (target {} Mbps @ {} fps), convert {:.1} ms, encode {:.1} ms, capture->sent {:.1} ms, key {} (req {}), dropped {}, audio {} {:.0} kbps{}",
                s.encoder,
                s.size.0,
                s.size.1,
                s.fps,
                s.mbps,
                s.target_mbps,
                s.target_fps,
                s.convert_ms,
                s.encode_ms,
                s.latency_ms,
                s.keyframes,
                s.keyframe_requests,
                s.frames_dropped,
                s.audio_on,
                s.audio_kbps,
                s.audio_error.map(|e| format!(" ({e})")).unwrap_or_default(),
            );
        }
    }
    session.stop();
    println!("end: {:?}", session.stats().phase);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("linux_cast only runs on Linux");
}
