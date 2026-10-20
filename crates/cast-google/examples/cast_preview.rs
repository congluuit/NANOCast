//! Path A without a TV: serves the live fMP4 stream of the primary monitor; open
//! http://127.0.0.1:47901/ in Chrome to watch it (the page shows the player's lag).
//!
//!     cargo run --release -p nanocast-cast --example cast_preview -- [seconds] [fps] [--no-audio]

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let nums: Vec<u64> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let secs = nums.first().copied().unwrap_or(60);
    let mut cfg = nanocast_cast::CastConfig::default();
    if let Some(&fps) = nums.get(1) {
        cfg.fps = fps as u32;
    }
    cfg.audio = !args.iter().any(|a| a == "--no-audio");
    let p = nanocast_cast::Preview::start(cfg, nanocast_cast::SourceSpec::Monitor(0), 47901)?;
    println!("open http://127.0.0.1:{}/ (stream: /live.mp4)", p.server.port);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(2));
        let s = p.stats();
        println!("{:.1} fps {:.2} Mbps clients={} {}", s.fps, s.mbps, p.server.connected.load(std::sync::atomic::Ordering::Relaxed), s.message);
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
