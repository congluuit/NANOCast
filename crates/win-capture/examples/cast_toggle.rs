//! Casts a window (or monitor 1) to a receiver and switches audio (or pause / black
//! screen) on/off every few seconds, printing the PC-side stats twice a second. Used to
//! check that toggling mid-cast does not disturb the video (pair it with a receiver that
//! measures frame gaps, see docs/benchmark.md "Phase 1b" and "Phase 2").
//!
//!   cargo run --release -p nanocast-win --example cast_toggle -- [--window "title part"] [--to 127.0.0.1]
//!       [--seconds 20] [--every 3] [--toggle audio|pause|blank] [--codec h264|hevc] [--pin 1234]
//!       [--bitrate 20] [--fixed]

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nanocast_core::protocol::{VideoCodec, CONTROL_PORT, VIDEO_PORT};
use nanocast_win::capture::Source;
use nanocast_win::session::{CastConfig, CastSession, Phase};
use nanocast_win::sources;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (mut window, mut to, mut seconds, mut every) = (None::<String>, "127.0.0.1".to_string(), 20u64, 3u64);
    let (mut toggle, mut codec, mut pin, mut bitrate, mut adaptive) = ("audio".to_string(), VideoCodec::H264, None, 20, true);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--fixed" {
            adaptive = false;
            continue;
        }
        let v = args.next().context("option needs a value")?;
        match a.as_str() {
            "--window" => window = Some(v),
            "--to" => to = v,
            "--seconds" => seconds = v.parse()?,
            "--every" => every = v.parse()?,
            "--toggle" => toggle = v,
            "--codec" => codec = VideoCodec::parse(&v).context("--codec h264|hevc")?,
            "--pin" => pin = Some(v),
            "--bitrate" => bitrate = v.parse()?,
            other => anyhow::bail!("unknown option {other}"),
        }
    }
    let source = match &window {
        Some(part) => {
            let w = sources::list_windows()
                .into_iter()
                .find(|w| w.title.contains(part.as_str()))
                .with_context(|| format!("no window titled *{part}*"))?;
            println!("window: {} ({})", w.title, w.process);
            Source::Window(w.hwnd)
        }
        None => Source::Monitor(1),
    };
    let ip: IpAddr = to.parse()?;
    let mut session = CastSession::start(CastConfig {
        video_addr: SocketAddr::new(ip, VIDEO_PORT),
        ctrl_port: CONTROL_PORT,
        source,
        fps: 60,
        bitrate_mbps: bitrate,
        encoder: None,
        codec,
        adaptive,
        pin,
        pace_mbps: 80,
        sender_name: "cast_toggle".into(),
        audio: false,
    });
    let start = Instant::now();
    let mut on = false;
    let mut next_toggle = start + Duration::from_secs(every);
    while start.elapsed() < Duration::from_secs(seconds) && session.is_active() {
        std::thread::sleep(Duration::from_millis(500));
        if Instant::now() >= next_toggle {
            on = !on;
            match toggle.as_str() {
                "pause" => session.set_paused(on),
                "blank" => session.set_blank(on),
                _ => session.set_audio(on),
            }
            println!("{:>5.1}s  --- {toggle} {} ---", start.elapsed().as_secs_f64(), if on { "ON" } else { "OFF" });
            next_toggle += Duration::from_secs(every);
        }
        let s = session.stats();
        if s.phase == Phase::Casting {
            println!(
                "{:>5.1}s  {:>5.1} fps  {:>5.1} Mbps  encode {:>4.1} ms  dropped {}  audio {} {:>5.1} kbps  {:?} {:?}  target {} Mbps {} fps {:?}{}{}",
                start.elapsed().as_secs_f64(),
                s.fps,
                s.mbps,
                s.encode_ms,
                s.frames_dropped,
                if s.audio_on { "on " } else { "off" },
                s.audio_kbps,
                s.codec,
                s.codec_fallback,
                s.target_mbps,
                s.target_fps,
                s.adapt_reason,
                if s.paused { "  PAUSED" } else if s.blank { "  BLANK" } else { "" },
                s.audio_error.map(|e| format!("  ERROR {e}")).unwrap_or_default()
            );
        }
    }
    let s = session.stats();
    session.stop();
    println!("end: {:?}, {} frames sent", s.phase, s.frames_sent);
    Ok(())
}
