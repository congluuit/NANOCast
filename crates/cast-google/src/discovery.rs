//! Google Cast receivers on the LAN: mDNS `_googlecast._tcp.local.`, TXT `fn` (friendly
//! name), `md` (model), `id` (receiver id); the service port is the TLS Cast channel (8009).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};

use crate::CastDevice;

pub const SERVICE_TYPE: &str = "_googlecast._tcp.local.";
pub const DEFAULT_PORT: u16 = 8009;

pub fn discover(timeout: Duration) -> Result<Vec<CastDevice>> {
    let mdns = ServiceDaemon::new().context("không khởi động được mDNS để tìm TV")?;
    let rx = mdns.browse(SERVICE_TYPE).context("mDNS browse")?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<CastDevice> = Vec::new();
    let mut seen = HashSet::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(info) = event {
            if !seen.insert(info.fullname.clone()) {
                continue;
            }
            if let Some(d) = to_device(&info) {
                // One device can answer on several interfaces / names: keep it once.
                if !found.iter().any(|f| f.id == d.id && !d.id.is_empty() || f.addr == d.addr && f.port == d.port) {
                    found.push(d);
                }
            }
        }
    }
    let _ = mdns.stop_browse(SERVICE_TYPE);
    let _ = mdns.shutdown();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

fn to_device(info: &ResolvedService) -> Option<CastDevice> {
    let addr = info.get_addresses_v4().into_iter().min()?;
    let txt = |k: &str| info.get_property_val_str(k).map(str::to_string);
    let id = txt("id").unwrap_or_default();
    Some(device_from_txt(txt("fn"), txt("md"), id, &info.fullname, addr, info.get_port()))
}

/// Builds a device from its TXT values; the instance name stands in for a missing `fn`.
pub fn device_from_txt(
    friendly: Option<String>,
    model: Option<String>,
    id: String,
    fullname: &str,
    addr: std::net::Ipv4Addr,
    port: u16,
) -> CastDevice {
    let instance = fullname.strip_suffix(&format!(".{SERVICE_TYPE}")).unwrap_or(fullname).to_string();
    CastDevice {
        name: friendly.filter(|s| !s.is_empty()).unwrap_or(instance),
        model: model.unwrap_or_default(),
        addr,
        port: if port == 0 { DEFAULT_PORT } else { port },
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_fields() {
        let d = device_from_txt(
            Some("Phong hoc 12".into()),
            Some("Chromecast".into()),
            "abc123".into(),
            "Chromecast-abc123._googlecast._tcp.local.",
            "192.168.1.30".parse().unwrap(),
            8009,
        );
        assert_eq!(d.name, "Phong hoc 12");
        assert_eq!(d.model, "Chromecast");
        assert_eq!(d.id, "abc123");
        assert_eq!(d.port, 8009);
        let d = device_from_txt(None, None, String::new(), "Chromecast-x._googlecast._tcp.local.", "10.0.0.2".parse().unwrap(), 0);
        assert_eq!(d.name, "Chromecast-x");
        assert_eq!(d.port, DEFAULT_PORT);
    }
}
