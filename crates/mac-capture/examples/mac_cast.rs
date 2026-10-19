//! Command-line test of the macOS sender, no GUI needed (run it on the Mac):
//!
//!   cargo run -p nanocast-mac --example mac_cast -- --list
//!   cargo run -p nanocast-mac --example mac_cast -- 192.168.1.50            # display 1, 30 s
//!   cargo run -p nanocast-mac --example mac_cast -- 192.168.1.50 window:1234 60 --audio
//!
//! Arguments: receiver IP, source (`1`, `2`... = display index, `window:<CGWindowID>` from
//! `--list`), seconds, `--audio` to send system sound. Prints stats twice a second.
//! The terminal app (Terminal, iTerm, VS Code) needs the Screen Recording permission.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::net::{IpAddr, SocketAddr};
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use nanocast_core::protocol::{CONTROL_PORT, VIDEO_PORT};
    use nanocast_mac::capture::Source;
    use nanocast_mac::session::{CastConfig, CastSession, Phase};
    use nanocast_mac::sources;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let audio = args.iter().any(|a| a == "--audio");
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    println!(
        "macOS audio supported: {}, Screen Recording permission: {}",
        sources::audio_supported(),
        sources::has_screen_recording_permission()
    );
    if args.iter().any(|a| a == "--list") || pos.is_empty() {
        if !sources::has_screen_recording_permission() {
            sources::request_screen_recording_permission();
        }
        for m in sources::list_monitors() {
            println!("display {}: {} {}x{}{}", m.index, m.name, m.width, m.height, if m.primary { " (main)" } else { "" });
        }
        for w in sources::list_windows() {
            println!("window:{} [{}] {}{}", w.hwnd, w.process, w.title, if w.minimized { " (off screen)" } else { "" });
        }
        if pos.is_empty() {
            println!("usage: mac_cast <receiver-ip> [1|window:<id>] [seconds] [--audio]");
        }
        return Ok(());
    }

    let ip: IpAddr = pos[0].parse().context("receiver IP")?;
    let source = match pos.get(1).map(|s| s.as_str()) {
        Some(s) if s.starts_with("window:") => Source::Window(s[7..].parse().context("window id")?),
        Some(s) => Source::Monitor(s.parse().context("display index")?),
        None => Source::Monitor(1),
    };
    let secs: u64 = pos.get(2).map(|s| s.parse()).transpose().context("seconds")?.unwrap_or(30);

    let mut session = CastSession::start(CastConfig {
        video_addr: SocketAddr::new(ip, VIDEO_PORT),
        ctrl_port: CONTROL_PORT,
        source,
        fps: 60,
        bitrate_mbps: 8,
        encoder: None,
        pace_mbps: 80,
        sender_name: "NANOCast mac_cast".into(),
        audio,
    });
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end && session.is_active() {
        std::thread::sleep(Duration::from_millis(500));
        let s = session.stats();
        if s.phase == Phase::Casting {
            println!(
                "{} {}x{}: {:.1} fps {:.2} Mbps, encode {:.1} ms, capture->sent {:.1} ms, key {} (req {}), dropped {}, audio {} {:.0} kbps{}",
                s.encoder,
                s.size.0,
                s.size.1,
                s.fps,
                s.mbps,
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

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_cast only runs on macOS");
}
