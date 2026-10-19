//! Drives a receiver's control channel by hand, for testing pause / black screen / PIN
//! without the GUI (video comes from `spike-send` or a real sender).
//!
//!   cargo run -p nanocast-core --example ctrl_probe -- 127.0.0.1:48323 [--pin 1234] [--codec hevc] \
//!       "start; sleep 3; pause on; sleep 3; pause off; blank on; sleep 3; blank off; sleep 2; stop"
//!
//! Commands (separated by `;`): `start`, `pause on|off`, `blank on|off`, `audio on|off`,
//! `stop`, `sleep SECONDS`. Every message from the receiver is printed as it arrives.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use nanocast_core::control::{self, ControlReader, Message};
use nanocast_core::protocol::PROTOCOL_VERSION;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args.next().context("usage: ctrl_probe HOST:PORT [--pin NNNN] [--codec h264|hevc] \"commands\"")?.parse()?;
    let (mut pin, mut codec, mut script) = (None, "h264".to_string(), String::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pin" => pin = Some(args.next().context("--pin NNNN")?),
            "--codec" => codec = args.next().context("--codec NAME")?,
            _ => script = a,
        }
    }

    let mut stream = control::connect(addr, Duration::from_secs(2))?;
    let mut reader = ControlReader::new(stream.try_clone()?);
    std::thread::spawn(move || loop {
        match reader.read() {
            Ok(Some(Message::Stats { fps, lost, decode_ms })) => println!("<- stats fps {fps} lost {lost} decode {decode_ms} ms"),
            Ok(Some(m)) => println!("<- {m:?}"),
            Ok(None) => {
                println!("<- (connection closed by the receiver)");
                break;
            }
            Err(e) => {
                println!("<- error: {e}");
                break;
            }
        }
    });

    let send = |stream: &mut std::net::TcpStream, m: Message| -> Result<()> {
        println!("-> {m:?}");
        control::write_msg(stream, &m).context("send")
    };
    send(&mut stream, Message::Hello { ver: PROTOCOL_VERSION.into(), name: "ctrl_probe".into(), pin })?;
    std::thread::sleep(Duration::from_millis(300));
    for cmd in script.split(';').map(str::trim).filter(|c| !c.is_empty()) {
        let (word, arg) = cmd.split_once(' ').map(|(w, a)| (w, a.trim())).unwrap_or((cmd, ""));
        let on = || match arg {
            "on" | "1" | "true" => Ok(true),
            "off" | "0" | "false" => Ok(false),
            _ => bail!("{cmd}: expected on/off"),
        };
        match word {
            "start" => send(&mut stream, Message::Start { w: 1920, h: 1080, fps: 60, codec: codec.clone(), audio: false })?,
            "pause" => send(&mut stream, Message::Pause { on: on()? })?,
            "blank" => send(&mut stream, Message::Blank { on: on()? })?,
            "audio" => send(&mut stream, Message::Audio { on: on()? })?,
            "stop" => send(&mut stream, Message::Stop)?,
            "sleep" => std::thread::sleep(Duration::from_secs_f64(arg.parse().context("sleep SECONDS")?)),
            _ => bail!("unknown command {cmd:?}"),
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}
