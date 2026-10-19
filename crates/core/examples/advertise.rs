//! Advertises a fake receiver over mDNS so the sender GUI can be tried without a TV:
//!   cargo run -p nanocast-core --example advertise -- "Phòng 12 TV" [seconds]
//! Only the announcement is fake; nothing listens on the ports.

use std::time::Duration;

use nanocast_core::discovery::Advertiser;
use nanocast_core::protocol::VIDEO_PORT;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "NANOCast test TV".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let _ad = Advertiser::new(&name, VIDEO_PORT, 1920, 1080)?;
    println!("advertising \"{name}\" for {secs} s");
    std::thread::sleep(Duration::from_secs(secs));
    Ok(())
}
