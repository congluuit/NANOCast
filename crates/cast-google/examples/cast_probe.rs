//! Lists Google Cast receivers on the LAN and, with `--status`, asks each one what it runs.
//!
//!     cargo run -p nanocast-cast --example cast_probe -- [seconds] [--status]

use std::net::SocketAddr;
use std::time::Duration;

use nanocast_cast::channel::{Channel, NS_RECEIVER, RECEIVER_ID};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs = args.iter().find_map(|a| a.parse::<u64>().ok()).unwrap_or(3);
    let status = args.iter().any(|a| a == "--status");
    let devices = nanocast_cast::discover(Duration::from_secs(secs))?;
    println!("{} Google Cast device(s)", devices.len());
    for d in &devices {
        println!("  {:<30} {:<24} {}:{}  id={}", d.name, d.model, d.addr, d.port, d.id);
        if status {
            match Channel::connect(SocketAddr::new(d.addr.into(), d.port)) {
                Ok((ch, _events)) => {
                    match ch.request(NS_RECEIVER, RECEIVER_ID, serde_json::json!({"type": "GET_STATUS"}), Duration::from_secs(5)) {
                        Ok(v) => println!("    {}", v["status"]),
                        Err(e) => println!("    GET_STATUS: {e:#}"),
                    }
                    ch.close(&[]);
                }
                Err(e) => println!("    connect: {e:#}"),
            }
        }
    }
    Ok(())
}
