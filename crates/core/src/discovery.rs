//! LAN discovery over mDNS/DNS-SD. Receivers advertise `_nanocast._udp.local.`
//! with TXT `w`, `h`, `codec`, `ver`, `ctrl` (control TCP port); the sender browses for them. No daemon,
//! no accounts, nothing leaves the LAN.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};

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
            if let Some(r) = to_receiver(&info) {
                found.push(r);
            }
        }
    }
    let _ = mdns.stop_browse(SERVICE_TYPE);
    let _ = mdns.shutdown();
    Ok(found)
}

/// A receiver typed in by hand ("192.168.1.20" or "192.168.1.20:48322"), for networks
/// that block mDNS. The control port is assumed to be the default.
pub fn manual_receiver(text: &str) -> Result<ReceiverInfo> {
    let text = text.trim();
    let (ip, port) = match text.rsplit_once(':') {
        Some((ip, port)) => (ip, port.parse::<u16>().context("invalid port")?),
        None => (text, crate::protocol::VIDEO_PORT),
    };
    let addr: Ipv4Addr = ip.parse().context("invalid IPv4 address")?;
    Ok(ReceiverInfo {
        name: addr.to_string(),
        addr,
        port,
        ctrl_port: CONTROL_PORT,
        width: None,
        height: None,
        codec: "h264".into(),
        version: String::new(),
    })
}

fn to_receiver(info: &ResolvedService) -> Option<ReceiverInfo> {
    let addr = info.get_addresses_v4().into_iter().min()?;
    let txt = |k: &str| info.get_property_val_str(k).map(str::to_string);
    Some(ReceiverInfo {
        name: instance_name(&info.fullname),
        addr,
        port: info.get_port(),
        ctrl_port: txt("ctrl").and_then(|v| v.parse().ok()).unwrap_or(CONTROL_PORT),
        width: txt("w").and_then(|v| v.parse().ok()),
        height: txt("h").and_then(|v| v.parse().ok()),
        codec: txt("codec").unwrap_or_else(|| "h264".into()),
        version: txt("ver").unwrap_or_default(),
    })
}

fn instance_name(fullname: &str) -> String {
    fullname.strip_suffix(&format!(".{SERVICE_TYPE}")).unwrap_or(fullname).to_string()
}

/// Keeps browsing in the background (for the GUI): the list follows receivers that
/// appear, change address, or say goodbye / expire, without blocking the caller.
pub struct Browser {
    mdns: ServiceDaemon,
    found: Arc<Mutex<BTreeMap<String, ReceiverInfo>>>,
}

impl Browser {
    pub fn new() -> Result<Self> {
        let mdns = ServiceDaemon::new().context("starting mDNS")?;
        let browser = Self { mdns, found: Arc::default() };
        browser.browse()?;
        Ok(browser)
    }

    fn browse(&self) -> Result<()> {
        let rx = self.mdns.browse(SERVICE_TYPE).context("mDNS browse")?;
        let found = self.found.clone();
        std::thread::Builder::new().name("mdns-browse".into()).spawn(move || {
            while let Ok(event) = rx.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if let Some(r) = to_receiver(&info) {
                            found.lock().unwrap().insert(info.fullname.clone(), r);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        found.lock().unwrap().remove(&fullname);
                    }
                    ServiceEvent::SearchStopped(_) => break,
                    _ => {}
                }
            }
        })?;
        Ok(())
    }

    /// Receivers currently known, sorted by name.
    pub fn receivers(&self) -> Vec<ReceiverInfo> {
        let mut v: Vec<_> = self.found.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
        v
    }

    /// Forgets everything and asks the network again (the "Rescan" button).
    pub fn rescan(&self) -> Result<()> {
        let _ = self.mdns.stop_browse(SERVICE_TYPE);
        self.found.lock().unwrap().clear();
        self.browse()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.mdns.stop_browse(SERVICE_TYPE);
        let _ = self.mdns.shutdown();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VIDEO_PORT;

    #[test]
    fn manual_entry() {
        let r = manual_receiver(" 192.168.1.20 ").unwrap();
        assert_eq!((r.addr, r.port, r.ctrl_port), (Ipv4Addr::new(192, 168, 1, 20), VIDEO_PORT, CONTROL_PORT));
        let r = manual_receiver("10.0.0.5:5000").unwrap();
        assert_eq!((r.addr, r.port), (Ipv4Addr::new(10, 0, 0, 5), 5000));
        assert!(manual_receiver("tv.local").is_err());
        assert!(manual_receiver("10.0.0.5:99999").is_err());
        assert!(manual_receiver("").is_err());
    }
}
