//! Receiver side of the control channel (PLAN.md section 4; mirrors `ControlServer.kt`):
//! TCP CONTROL_PORT, one JSON object per line (`nanocast_core::control`).
//!
//! One sender at a time: a new connection replaces the previous one (the PC may have gone
//! away without closing). `caps` answers `hello`, `stats` goes out every second (`lost` =
//! packets lost since the previous report), `keyframe_request` at most once per 500 ms.
//! Every write happens on one writer thread, so the network and decoder threads never
//! block on TCP.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nanocast_core::control::{write_msg, ControlReader, Message};
use nanocast_core::protocol::PROTOCOL_VERSION;

use crate::stats::{RateMeter, Stats};

const STATS_EVERY: Duration = Duration::from_secs(1);
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// What the current sender told us (for the overlay / idle screen).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub connected: bool,
    /// Sender name from `hello` (or its IP).
    pub sender: String,
    /// `start` parameters, `None` before `start` / after `stop`.
    pub stream: Option<(u32, u32, u32)>,
    pub audio: bool,
    pub stopped: bool,
}

struct Inner {
    name: String,
    max_w: u32,
    max_h: u32,
    stats: Arc<Stats>,
    client: Mutex<Option<TcpStream>>,
    /// Bumped on every accepted connection, so an old reader cannot clear a newer client.
    generation: AtomicU64,
    connected: AtomicBool,
    last_keyframe: Mutex<Option<Instant>>,
    session: Mutex<Session>,
    tx: Mutex<Sender<Message>>,
}

#[derive(Clone)]
pub struct ControlServer {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl ControlServer {
    pub fn start(bind: SocketAddr, name: &str, max_w: u32, max_h: u32, stats: Arc<Stats>) -> Result<Self> {
        let listener = TcpListener::bind(bind).with_context(|| format!("binding control TCP {bind}"))?;
        let addr = listener.local_addr()?;
        let (tx, rx) = channel::<Message>();
        let inner = Arc::new(Inner {
            name: name.to_string(),
            max_w,
            max_h,
            stats,
            client: Mutex::new(None),
            generation: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            last_keyframe: Mutex::new(None),
            session: Mutex::new(Session::default()),
            tx: Mutex::new(tx),
        });

        let w = inner.clone();
        std::thread::Builder::new().name("ctrl-tx".into()).spawn(move || {
            let mut meter = RateMeter::new(&w.stats);
            let mut next_stats = Instant::now() + STATS_EVERY;
            loop {
                let left = next_stats.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(msg) => w.write(&msg),
                    Err(RecvTimeoutError::Timeout) => {
                        next_stats += STATS_EVERY;
                        let r = meter.sample(&w.stats);
                        if w.connected.load(Relaxed) {
                            let msg = Message::Stats {
                                fps: (r.fps_decoded * 10.0).round() / 10.0,
                                lost: r.lost,
                                decode_ms: (r.decode_ms * 100.0).round() / 100.0,
                            };
                            w.write(&msg);
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;

        let a = inner.clone();
        std::thread::Builder::new().name("ctrl-accept".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => a.clone().accept(s),
                    Err(e) => log::warn!("control accept: {e}"),
                }
            }
        })?;
        log::info!("control channel listening on TCP {addr}");
        Ok(Self { inner, addr })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Asks the sender for an IDR. Cheap and thread-safe; rate-limited to one per 500 ms.
    pub fn request_keyframe(&self, reason: &str) {
        if !self.inner.connected.load(Relaxed) {
            return;
        }
        {
            let mut last = self.inner.last_keyframe.lock().unwrap();
            let now = Instant::now();
            if last.is_some_and(|t| now - t < KEYFRAME_MIN_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        self.inner.stats.keyframe_requests.fetch_add(1, Relaxed);
        log::info!("keyframe_request ({reason})");
        self.inner.send(Message::KeyframeRequest { reason: reason.into() });
    }

    pub fn session(&self) -> Session {
        self.inner.session.lock().unwrap().clone()
    }
}

impl Inner {
    fn send(&self, msg: Message) {
        let _ = self.tx.lock().unwrap().send(msg);
    }

    fn accept(self: Arc<Self>, sock: TcpStream) {
        let peer = sock.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "?".into());
        let _ = sock.set_nodelay(true);
        let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
        let Ok(writer) = sock.try_clone() else { return };
        let generation = self.generation.fetch_add(1, Relaxed) + 1;
        if let Some(old) = self.client.lock().unwrap().replace(writer) {
            let _ = old.shutdown(Shutdown::Both); // its reader thread ends on the closed socket
        }
        *self.last_keyframe.lock().unwrap() = None;
        self.connected.store(true, Relaxed);
        *self.session.lock().unwrap() = Session { connected: true, sender: peer.clone(), ..Default::default() };
        log::info!("sender connected from {peer}");
        let me = self.clone();
        let _ = std::thread::Builder::new().name("ctrl-rx".into()).spawn(move || {
            if let Err(e) = me.read_loop(sock, &peer) {
                log::debug!("control read from {peer}: {e}");
            }
            if me.generation.load(Relaxed) == generation {
                me.client.lock().unwrap().take();
                me.connected.store(false, Relaxed);
                me.session.lock().unwrap().connected = false;
            }
            log::info!("sender {peer} disconnected");
        });
    }

    fn read_loop(&self, sock: TcpStream, peer: &str) -> io::Result<()> {
        let mut reader = ControlReader::new(sock);
        loop {
            let msg = match reader.read() {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    log::warn!("{e}");
                    continue;
                }
                Err(e) => return Err(e),
            };
            match msg {
                Message::Hello { ver, name } => {
                    let sender = if name.is_empty() { peer.to_string() } else { name };
                    log::info!("hello from {sender} (v{ver})");
                    self.session.lock().unwrap().sender = sender;
                    self.send(Message::Caps {
                        ver: PROTOCOL_VERSION.into(),
                        name: self.name.clone(),
                        max_w: self.max_w,
                        max_h: self.max_h,
                        codecs: vec!["h264".into()],
                        // No reorder buffer: OpenH264 returns each picture from the same call.
                        low_latency: true,
                    });
                }
                Message::Start { w, h, fps, codec, audio } => {
                    log::info!("start: {w}x{h}@{fps} {codec} audio {}", if audio { "on" } else { "off" });
                    let mut s = self.session.lock().unwrap();
                    s.stream = Some((w, h, fps));
                    s.audio = audio;
                    s.stopped = false;
                }
                Message::Audio { on } => {
                    log::info!("audio {}", if on { "on" } else { "off" });
                    self.session.lock().unwrap().audio = on;
                }
                Message::Stop => {
                    log::info!("stop");
                    let mut s = self.session.lock().unwrap();
                    s.stream = None;
                    s.stopped = true;
                }
                other => log::debug!("ignored control message {other:?}"),
            }
        }
    }

    /// Writer thread only. A failed write closes the socket; the reader thread then cleans up.
    fn write(&self, msg: &Message) {
        let mut guard = self.client.lock().unwrap();
        let Some(sock) = guard.as_mut() else { return };
        if let Err(e) = write_msg(sock, msg) {
            log::warn!("control write failed: {e}");
            let _ = sock.shutdown(Shutdown::Both);
            guard.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocast_core::control;

    fn wait_for(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !f() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn hello_caps_keyframe_stats_and_replacement() {
        let stats = Arc::new(Stats::new());
        let server = ControlServer::start("127.0.0.1:0".parse().unwrap(), "Test TV", 1920, 1080, stats.clone()).unwrap();
        // Not connected: requests are dropped.
        server.request_keyframe("packet_loss");
        assert_eq!(stats.keyframe_requests.load(Relaxed), 0);

        let mut s = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
        let mut r = ControlReader::new(s.try_clone().unwrap());
        write_msg(&mut s, &Message::Hello { ver: "0.1".into(), name: "Teacher".into() }).unwrap();
        match r.read().unwrap() {
            Some(Message::Caps { name, max_w, codecs, .. }) => {
                assert_eq!((name.as_str(), max_w, codecs), ("Test TV", 1920, vec!["h264".to_string()]))
            }
            other => panic!("expected caps, got {other:?}"),
        }
        write_msg(&mut s, &Message::Start { w: 1280, h: 720, fps: 30, codec: "h264".into(), audio: true }).unwrap();
        wait_for(|| server.session().stream == Some((1280, 720, 30)));
        assert_eq!(server.session().sender, "Teacher");

        // Rate limit: two requests back to back -> one message.
        server.request_keyframe("packet_loss");
        server.request_keyframe("packet_loss");
        assert_eq!(r.read().unwrap(), Some(Message::KeyframeRequest { reason: "packet_loss".into() }));
        assert_eq!(stats.keyframe_requests.load(Relaxed), 1);
        // Next message is the 1 s stats report, not a second keyframe_request.
        assert!(matches!(r.read().unwrap(), Some(Message::Stats { .. })));

        write_msg(&mut s, &Message::Stop).unwrap();
        wait_for(|| server.session().stopped);

        // A second sender replaces the first, whose socket gets closed.
        let mut s2 = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
        write_msg(&mut s2, &Message::Hello { ver: "0.1".into(), name: "Other".into() }).unwrap();
        let mut r2 = ControlReader::new(s2.try_clone().unwrap());
        assert!(matches!(r2.read().unwrap(), Some(Message::Caps { .. })));
        loop {
            match r.read() {
                Ok(None) | Err(_) => break,
                Ok(Some(Message::Stats { .. })) => continue,
                Ok(Some(m)) => panic!("old sender got {m:?}"),
            }
        }
        assert!(server.session().connected);
        drop((s2, r2));
        wait_for(|| !server.session().connected);
    }
}
