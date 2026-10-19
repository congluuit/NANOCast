//! LAN discovery over mDNS/DNS-SD. Receivers advertise `_nanocast._udp.local.`
//! with TXT `w`, `h`, `codec`, `ver`, `ctrl` (control TCP port); the sender browses for them. No daemon,
//! no accounts, nothing leaves the LAN.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::protocol::{CONTROL_PORT, PROTOCOL_VERSION, SERVICE_TYPE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverInfo {
    /// Instance name as shown to the user (e.g. "Living room TV").
    pub name: String,
    pub addr: Ipv4Addr,
    /// UDP video port (the service port).
    pub port: u16,
    /// TCP control port from TXT `ctrl`; CONTROL_PORT when the receiver does not say.
    pub ctrl_port: u16,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: String,
    pub version: String,
}

/// Browses the LAN for receivers for `timeout`, returning each one once.
pub fn discover(timeout: Duration) -> Result<Vec<ReceiverInfo>> {
    let mdns = ServiceDaemon::new().context("starting mDNS")?;
    let rx = mdns.browse(SERVICE_TYPE).context("mDNS browse")?;
    let deadline = Instant::now() + timeout;
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(info) = event {
            if !seen.insert(info.fullname.clone()) {
                continue;
            }
            let Some(addr) = info.get_addresses_v4().into_iter().min() else { continue };
            let name = info
                .fullname
                .strip_suffix(&format!(".{SERVICE_TYPE}"))
                .unwrap_or(&info.fullname)
                .to_string();
            let txt = |k: &str| info.get_property_val_str(k).map(str::to_string);
            found.push(ReceiverInfo {
                name,
                addr,
                port: info.get_port(),
                ctrl_port: txt("ctrl").and_then(|v| v.parse().ok()).unwrap_or(CONTROL_PORT),
                width: txt("w").and_then(|v| v.parse().ok()),
                height: txt("h").and_then(|v| v.parse().ok()),
                codec: txt("codec").unwrap_or_else(|| "h264".into()),
                version: txt("ver").unwrap_or_default(),
            });
        }
    }
    let _ = mdns.stop_browse(SERVICE_TYPE);
    let _ = mdns.shutdown();
    Ok(found)
}

/// Advertises this machine as a receiver until dropped (for the desktop receiver).
pub struct Advertiser {
    mdns: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn new(name: &str, port: u16, width: u32, height: u32) -> Result<Self> {
        let mdns = ServiceDaemon::new().context("starting mDNS")?;
        let host = format!("{}.local.", sanitize(name));
        let mut txt = HashMap::new();
        txt.insert("w".to_string(), width.to_string());
        txt.insert("h".to_string(), height.to_string());
        txt.insert("codec".to_string(), "h264".to_string());
        txt.insert("ver".to_string(), PROTOCOL_VERSION.to_string());
        txt.insert("ctrl".to_string(), CONTROL_PORT.to_string());
        let info = ServiceInfo::new(SERVICE_TYPE, name, &host, "", port, txt)
            .context("service info")?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        mdns.register(info).context("mDNS register")?;
        Ok(Self { mdns, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.mdns.unregister(&self.fullname);
        let _ = self.mdns.shutdown();
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    if s.is_empty() { "nanocast".into() } else { s }
}
