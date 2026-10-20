//! Google Cast (Chromecast built-in) from the GUI thread without blocking it: discovery
//! runs on its own thread every 3 s, and connecting / stopping (TLS, launching the receiver
//! app on the TV) happen on short-lived threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanocast_cast::{CastConfig, CastDevice, CastSession, SourceSpec};

const EVERY: Duration = Duration::from_secs(3);
const BROWSE_FOR: Duration = Duration::from_millis(2500);

/// Keeps the list of Google Cast devices on the LAN fresh.
pub struct Scanner {
    devices: Arc<Mutex<Vec<CastDevice>>>,
    quit: Arc<AtomicBool>,
}

impl Scanner {
    pub fn start() -> Self {
        let devices = Arc::new(Mutex::new(Vec::new()));
        let quit = Arc::new(AtomicBool::new(false));
        let (d, q) = (devices.clone(), quit.clone());
        let spawned = std::thread::Builder::new().name("gcast-discovery".into()).spawn(move || {
            let mut logged_error = false;
            while !q.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                match nanocast_cast::discover(BROWSE_FOR) {
                    Ok(found) => *d.lock().unwrap() = found,
                    Err(e) if !logged_error => {
                        log::warn!("Google Cast discovery: {e:#}");
                        logged_error = true;
                    }
                    Err(_) => {}
                }
                while t0.elapsed() < EVERY && !q.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        if let Err(e) = spawned {
            log::warn!("Google Cast discovery thread: {e}");
        }
        Self { devices, quit }
    }

    pub fn devices(&self) -> Vec<CastDevice> {
        self.devices.lock().unwrap().clone()
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

/// A connection being set up in the background.
pub struct Pending {
    rx: Receiver<Result<CastSession, String>>,
}

impl Pending {
    /// `Some` once connecting finished (the session, or the error text).
    pub fn poll(&self) -> Option<Result<CastSession, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("Google Cast: connecting thread ended".into())),
        }
    }
}

pub fn start(device: CastDevice, config: CastConfig, source: SourceSpec) -> Pending {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("gcast-start".into()).spawn(move || {
        let result = CastSession::start(device, config, source).map_err(|e| format!("{e:#}"));
        if let Err(mpsc::SendError(Ok(session))) = tx.send(result) {
            // Cancelled meanwhile: close the receiver app on the TV again.
            session.stop();
        }
    });
    if let Err(e) = spawned {
        log::warn!("Google Cast start thread: {e}");
    }
    Pending { rx }
}

/// Stops without freezing the window.
pub fn stop(session: CastSession) {
    if let Err(e) = std::thread::Builder::new().name("gcast-stop".into()).spawn(move || session.stop()) {
        log::warn!("Google Cast stop thread: {e}");
    }
}

/// Whether a start error means "this build cannot do Google Cast" rather than a network
/// problem worth retrying.
pub fn is_unsupported(error: &str) -> bool {
    let e = error.to_lowercase();
    ["chưa được cài đặt", "chưa hỗ trợ", "not supported", "unsupported", "not implemented", "not available"].iter().any(|k| e.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_errors() {
        assert!(is_unsupported("Google Cast chưa được cài đặt trong bản này (Phòng 12)"));
        assert!(is_unsupported("Cast Streaming not supported by this receiver"));
        assert!(!is_unsupported("connecting to 192.168.1.20:8009: timed out"));
    }
}
