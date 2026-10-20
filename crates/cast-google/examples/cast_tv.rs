//! Casts the primary monitor to a Google Cast device for a while, printing stats.
//!
//!     cargo run --release -p nanocast-cast --example cast_tv -- [name-or-ip] [seconds]
//!
//! `NANOCAST_CAST_MODE=mirroring|media` forces a path.

use std::time::{Duration, Instant};

use nanocast_cast::{CastConfig, CastSession, SourceSpec};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs = args.iter().find_map(|a| a.parse::<u64>().ok()).unwrap_or(30);
    let want = args.iter().find(|a| a.parse::<u64>().is_err()).cloned();
    let devices = nanocast_cast::discover(Duration::from_secs(3))?;
    let device = devices
        .into_iter()
        .find(|d| want.as_deref().is_none_or(|w| d.name.contains(w) || d.addr.to_string() == w))
        .ok_or_else(|| anyhow::anyhow!("no Google Cast device found"))?;
    println!("casting to {} ({}, {})", device.name, device.model, device.addr);
    let t = Instant::now();
    let session = CastSession::start(device, CastConfig::default(), SourceSpec::Monitor(0))?;
    println!("started in {:.1} s", t.elapsed().as_secs_f32());
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_secs(2));
        let s = session.stats();
        println!(
            "{:?} {:.1} fps {:.2} Mbps delay={:?} ms connected={} | {}",
            s.mode, s.fps, s.mbps, s.latency_ms, s.connected, s.message
        );
    }
    session.stop();
    Ok(())
}
