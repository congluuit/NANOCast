//! Receiver side of the control channel (PLAN.md section 4; mirrors `ControlServer.kt`):
//! TCP CONTROL_PORT, one JSON object per line (`nanocast_core::control`).
//!
//! One sender at a time: a sender whose `hello` is accepted replaces the previous one (the
//! PC may have gone away without closing). `caps` answers `hello`, `stats` goes out every
//! second (`lost` = packets lost since the previous report), `keyframe_request` at most
//! once per 500 ms. Every write to the current sender happens on one writer thread, so the
//! network and decoder threads never block on TCP.
//!
//! With a 4-digit code (`--pin`), a `hello` without the right `pin` gets
//! `error {code: "bad_pin"}` and the connection is closed without touching the current
//! sender; video and audio UDP are then only accepted from the accepted sender's address
//! (`allows`). This receiver decodes H.264 only, so `caps.codecs` is `["h264"]` and senders
//! fall back from HEVC by themselves.

use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nanocast_core::control::{write_msg, ControlReader, Message, ERR_BAD_PIN};
use nanocast_core::protocol::{VideoCodec, PROTOCOL_VERSION};

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
    /// `start.codec` when it is one this receiver cannot decode (HEVC).
    pub unsupported_codec: Option<String>,
    pub audio: bool,
    /// `pause {on}`: keep the last picture. `blank {on}`: show black.
    pub paused: bool,
    pub blank: bool,
    pub stopped: bool,
    /// `hello` refusals since start (wrong or missing code).
    pub rejected: u64,
}

struct Inner {
    name: String,
    max_w: u32,
    max_h: u32,
    /// The 4-digit code senders must give, if enabled.
    pin: Option<String>,
    stats: Arc<Stats>,
    client: Mutex<Option<TcpStream>>,
    /// Address of the accepted sender (UDP filter when `pin` is set).
    authorized: Mutex<Option<IpAddr>>,
    /// Bumped on every accepted sender, so an old reader cannot clear a newer client.
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
    pub fn start(bind: SocketAddr, name: &str, max_w: u32, max_h: u32, pin: Option<String>, stats: Arc<Stats>) -> Result<Self> {
        let listener = TcpListener::bind(bind).with_context(|| format!("binding control TCP {bind}"))?;
        let addr = listener.local_addr()?;
        let (tx, rx) = channel::<Message>();
        let inner = Arc::new(Inner {
            name: name.to_string(),
            max_w,
            max_h,
            pin,
            stats,
            client: Mutex::new(None),
            authorized: Mutex::new(None),
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
        log::info!(
            "control channel listening on TCP {addr}{}",
            if inner.pin.is_some() { " (4-digit code required)" } else { "" }
        );
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

    /// Whether UDP from `ip` may be shown / played: always without a code, otherwise only
    /// from the sender that gave the right one.
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.inner.pin.is_none() || *self.inner.authorized.lock().unwrap() == Some(ip)
    }
}

impl Inner {
    fn send(&self, msg: Message) {
        let _ = self.tx.lock().unwrap().send(msg);
    }

    fn accept(self: Arc<Self>, sock: TcpStream) {
        let peer = sock.peer_addr().ok();
        let _ = sock.set_nodelay(true);
        let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
        let me = self.clone();
        let _ = std::thread::Builder::new().name("ctrl-rx".into()).spawn(move || {
            let label = peer.map(|a| a.ip().to_string()).unwrap_or_else(|| "?".into());
            let mut generation = None;
            if let Err(e) = me.read_loop(sock, peer.map(|a| a.ip()), &label, &mut generation) {
                log::debug!("control read from {label}: {e}");
            }
            if let Some(g) = generation {
                if me.generation.load(Relaxed) == g {
                    me.client.lock().unwrap().take();
                    me.authorized.lock().unwrap().take();
                    me.connected.store(false, Relaxed);
                    let mut s = me.session.lock().unwrap();
                    (s.connected, s.paused, s.blank) = (false, false, false);
                }
                log::info!("sender {label} disconnected");
            }
        });
    }

    /// Makes `sock` the current sender (after an accepted `hello`); the previous one is closed.
    fn adopt(&self, sock: &TcpStream, ip: Option<IpAddr>, sender: String) -> io::Result<u64> {
        let writer = sock.try_clone()?;
        let generation = self.generation.fetch_add(1, Relaxed) + 1;
        if let Some(old) = self.client.lock().unwrap().replace(writer) {
            let _ = old.shutdown(Shutdown::Both); // its reader thread ends on the closed socket
        }
        *self.authorized.lock().unwrap() = ip;
        *self.last_keyframe.lock().unwrap() = None;
        self.connected.store(true, Relaxed);
        let mut s = self.session.lock().unwrap();
        let rejected = s.rejected;
        *s = Session { connected: true, sender, rejected, ..Default::default() };
        Ok(generation)
    }

    fn read_loop(&self, sock: TcpStream, ip: Option<IpAddr>, peer: &str, generation: &mut Option<u64>) -> io::Result<()> {
        let mut reader = ControlReader::new(sock.try_clone()?);
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
            if let Message::Hello { ver, name, pin } = &msg {
                let sender = if name.is_empty() { peer.to_string() } else { name.clone() };
                if let Some(want) = &self.pin {
                    if pin.as_deref() != Some(want.as_str()) {
                        log::warn!("hello from {sender} ({peer}) refused: {}", if pin.is_some() { "wrong code" } else { "no code" });
                        self.session.lock().unwrap().rejected += 1;
                        let mut s = &sock;
                        let refusal = Message::Error { code: ERR_BAD_PIN.into(), message: "4-digit code required".into() };
                        let _ = write_msg(&mut s, &refusal);
                        let _ = sock.shutdown(Shutdown::Both);
                        return Ok(());
                    }
                }
                log::info!("hello from {sender} (v{ver}) at {peer}");
                *generation = Some(self.adopt(&sock, ip, sender)?);
                self.send(Message::Caps {
                    ver: PROTOCOL_VERSION.into(),
                    name: self.name.clone(),
                    max_w: self.max_w,
                    max_h: self.max_h,
                    // OpenH264 only: no HEVC here, senders fall back to H.264.
                    codecs: vec![VideoCodec::H264.as_str().into()],
                    // No reorder buffer: OpenH264 returns each picture from the same call.
                    low_latency: true,
                });
                continue;
            }
            // Nothing but `hello` counts before a sender is accepted.
            if generation.is_none_or(|g| self.generation.load(Relaxed) != g) {
                log::debug!("ignored {msg:?} from {peer} (no accepted hello)");
                continue;
            }
            match msg {
                Message::Start { w, h, fps, codec, audio } => {
                    log::info!("start: {w}x{h}@{fps} {codec} audio {}", if audio { "on" } else { "off" });
                    let mut s = self.session.lock().unwrap();
                    s.stream = Some((w, h, fps));
                    s.unsupported_codec = (VideoCodec::parse(&codec) != Some(VideoCodec::H264)).then(|| {
                        log::warn!("sender started {codec}; this receiver decodes H.264 only");
                        codec.clone()
                    });
                    s.audio = audio;
                    (s.paused, s.blank, s.stopped) = (false, false, false);
                }
                Message::Audio { on } => {
                    log::info!("audio {}", if on { "on" } else { "off" });
                    self.session.lock().unwrap().audio = on;
                }
                Message::Pause { on } => {
                    log::info!("pause {}", if on { "on" } else { "off" });
                    self.session.lock().unwrap().paused = on;
                }
                Message::Blank { on } => {
                    log::info!("blank {}", if on { "on" } else { "off" });
                    self.session.lock().unwrap().blank = on;
                }
                Message::Stop => {
                    log::info!("stop");
                    let mut s = self.session.lock().unwrap();
                    s.stream = None;
                    (s.paused, s.blank, s.stopped) = (false, false, true);
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

    fn hello(name: &str, pin: Option<&str>) -> Message {
        Message::Hello { ver: "0.1".into(), name: name.into(), pin: pin.map(str::to_string) }
    }

    #[test]
    fn hello_caps_keyframe_stats_and_replacement() {
        let stats = Arc::new(Stats::new());
        let server = ControlServer::start("127.0.0.1:0".parse().unwrap(), "Test TV", 1920, 1080, None, stats.clone()).unwrap();
        // Not connected: requests are dropped.
        server.request_keyframe("packet_loss");
        assert_eq!(stats.keyframe_requests.load(Relaxed), 0);
        assert!(server.allows("10.1.2.3".parse().unwrap()), "no code: every address");

        let mut s = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
        let mut r = ControlReader::new(s.try_clone().unwrap());
        write_msg(&mut s, &hello("Teacher", None)).unwrap();
        match r.read().unwrap() {
            Some(Message::Caps { name, max_w, codecs, .. }) => {
                assert_eq!((name.as_str(), max_w, codecs), ("Test TV", 1920, vec!["h264".to_string()]))
            }
            other => panic!("expected caps, got {other:?}"),
        }
        write_msg(&mut s, &Message::Start { w: 1280, h: 720, fps: 30, codec: "h264".into(), audio: true }).unwrap();
        wait_for(|| server.session().stream == Some((1280, 720, 30)));
        assert_eq!(server.session().sender, "Teacher");
        assert_eq!(server.session().unsupported_codec, None);

        write_msg(&mut s, &Message::Pause { on: true }).unwrap();
        write_msg(&mut s, &Message::Blank { on: true }).unwrap();
        wait_for(|| server.session().paused && server.session().blank);
        write_msg(&mut s, &Message::Pause { on: false }).unwrap();
        wait_for(|| !server.session().paused && server.session().blank);

        // Rate limit: two requests back to back -> one message.
        server.request_keyframe("packet_loss");
        server.request_keyframe("packet_loss");
        assert_eq!(r.read().unwrap(), Some(Message::KeyframeRequest { reason: "packet_loss".into() }));
        assert_eq!(stats.keyframe_requests.load(Relaxed), 1);
        // Next message is the 1 s stats report, not a second keyframe_request.
        assert!(matches!(r.read().unwrap(), Some(Message::Stats { .. })));

        write_msg(&mut s, &Message::Stop).unwrap();
        wait_for(|| server.session().stopped && !server.session().blank);

        // A second sender replaces the first, whose socket gets closed.
        let mut s2 = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
        write_msg(&mut s2, &hello("Other", None)).unwrap();
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
        // HEVC is not decoded here: noted for the overlay.
        write_msg(&mut s2, &Message::Start { w: 1920, h: 1080, fps: 60, codec: "hevc".into(), audio: false }).unwrap();
        wait_for(|| server.session().unsupported_codec.as_deref() == Some("hevc"));
        drop((s2, r2));
        wait_for(|| !server.session().connected);
    }

    #[test]
    fn pin_required() {
        let stats = Arc::new(Stats::new());
        let server =
            ControlServer::start("127.0.0.1:0".parse().unwrap(), "Class TV", 1920, 1080, Some("0427".into()), stats).unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(!server.allows(local), "nobody before a correct hello");

        // Accepted sender.
        let mut ok = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
        let mut rok = ControlReader::new(ok.try_clone().unwrap());
        write_msg(&mut ok, &hello("Teacher", Some("0427"))).unwrap();
        assert!(matches!(rok.read().unwrap(), Some(Message::Caps { .. })));
        wait_for(|| server.allows(local));
        assert!(!server.allows("192.168.1.50".parse().unwrap()));

        // Wrong code and no code: error, closed, the teacher stays connected.
        for pin in [Some("1111"), None] {
            let mut bad = control::connect(server.local_addr(), Duration::from_secs(2)).unwrap();
            let mut rbad = ControlReader::new(bad.try_clone().unwrap());
            write_msg(&mut bad, &hello("Student", pin)).unwrap();
            assert_eq!(rbad.read().unwrap(), Some(Message::Error { code: ERR_BAD_PIN.into(), message: "4-digit code required".into() }));
            assert!(matches!(rbad.read(), Ok(None) | Err(_)), "closed after the refusal");
            // A start sent anyway changes nothing.
            let _ = write_msg(&mut bad, &Message::Blank { on: true });
        }
        assert_eq!(server.session().rejected, 2);
        assert_eq!(server.session().sender, "Teacher");
        assert!(server.session().connected && !server.session().blank);
        assert!(server.allows(local));

        drop((ok, rok));
        wait_for(|| !server.session().connected);
        assert!(!server.allows(local), "the code is needed again after the sender left");
    }
}
