//! Cast v2 channel: TLS to ip:8009, length-prefixed `CastMessage`s, JSON payloads.
//!
//! One I/O thread owns the TLS stream (rustls streams cannot be split): it writes queued
//! messages, reads with a short timeout, answers PING with PONG, sends PING every 5 s and
//! routes replies to waiting `request`s by `requestId`. Everything else goes to `events`.
//!
//! The device certificate is self-signed (signed by Google's device CA, which only the
//! optional device-auth step checks): the TLS verifier accepts any certificate. On a LAN
//! the worst a spoofed "TV" could do is receive the picture the user chose to cast.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use log::{debug, info, warn};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{json, Value};

use crate::proto::{CastMessage, MAX_MESSAGE};

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const NS_WEBRTC: &str = "urn:x-cast:com.google.cast.webrtc";

pub const SENDER_ID: &str = "sender-0";
pub const RECEIVER_ID: &str = "receiver-0";

const PING_EVERY: Duration = Duration::from_secs(5);
/// Nothing at all from the device for this long: the channel is dead.
const DEAD_AFTER: Duration = Duration::from_secs(16);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Messages the I/O thread does not route to a `request`.
pub type Events = Receiver<CastMessage>;

struct Shared {
    pending: Mutex<HashMap<u64, SyncSender<Value>>>,
    closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
}

pub struct Channel {
    out: Sender<CastMessage>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    thread: Option<JoinHandle<()>>,
    /// This PC's address as the device sees it (the HTTP server URL uses it).
    pub local_ip: IpAddr,
    pub peer: SocketAddr,
}

impl Channel {
    /// Opens TLS and the virtual connection to `receiver-0`. Returns the channel and the
    /// stream of unsolicited messages (statuses, CLOSE, webrtc ANSWER...).
    pub fn connect(peer: SocketAddr) -> Result<(Self, Events)> {
        let tcp = TcpStream::connect_timeout(&peer, CONNECT_TIMEOUT)
            .with_context(|| format!("TV không trả lời ở {peer}, kiểm tra TV đang bật và cùng Wi-Fi với máy tính"))?;
        let local_ip = tcp.local_addr()?.ip();
        tcp.set_nodelay(true)?;
        tcp.set_write_timeout(Some(Duration::from_secs(3)))?;
        tcp.set_read_timeout(Some(Duration::from_millis(30)))?;

        let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth();
        let name = ServerName::IpAddress(peer.ip().into());
        let conn = ClientConnection::new(Arc::new(config), name)?;
        let mut tls = StreamOwned::new(conn, tcp);
        // Handshake now so an incompatible device fails here with a clear message.
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while tls.conn.is_handshaking() {
            match tls.conn.complete_io(&mut tls.sock) {
                Ok(_) => {}
                Err(e) if is_timeout(&e) && Instant::now() < deadline => {}
                Err(e) => return Err(e).context("bắt tay TLS với TV thất bại (thiết bị không hỗ trợ Google Cast?)"),
            }
        }
        debug!("cast TLS up to {peer} ({:?})", tls.conn.protocol_version());

        let shared = Arc::new(Shared { pending: Mutex::default(), closed: AtomicBool::new(false), close_reason: Mutex::default() });
        let (out_tx, out_rx) = channel::<CastMessage>();
        let (ev_tx, ev_rx) = channel::<CastMessage>();
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new().name("cast-channel".into()).spawn(move || {
                let reason = match io_loop(&mut tls, &out_rx, &ev_tx, &shared) {
                    Ok(()) => "đã đóng".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                debug!("cast channel ended: {reason}");
                *shared.close_reason.lock().unwrap() = Some(reason);
                shared.closed.store(true, Ordering::Relaxed);
                shared.pending.lock().unwrap().clear();
                let _ = tls.sock.shutdown(std::net::Shutdown::Both);
            })?
        };
        let ch = Self { out: out_tx, shared, next_id: AtomicU64::new(1), thread: Some(thread), local_ip, peer };
        ch.send(NS_CONNECTION, RECEIVER_ID, json!({"type": "CONNECT", "origin": {}, "userAgent": "NANOCast"}));
        Ok((ch, ev_rx))
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    pub fn close_reason(&self) -> Option<String> {
        self.shared.close_reason.lock().unwrap().clone()
    }

    /// Queues a JSON message (fire and forget).
    pub fn send(&self, namespace: &str, destination: &str, payload: Value) {
        let msg = CastMessage::json(SENDER_ID, destination, namespace, &payload.to_string());
        debug!("cast -> {destination} {namespace}: {}", payload);
        let _ = self.out.send(msg);
    }

    /// Sends `payload` with a fresh `requestId` and waits for the reply carrying it.
    pub fn request(&self, namespace: &str, destination: &str, mut payload: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        payload["requestId"] = json!(id);
        let (tx, rx) = sync_channel(1);
        self.shared.pending.lock().unwrap().insert(id, tx);
        let kind = payload["type"].as_str().unwrap_or("?").to_string();
        self.send(namespace, destination, payload);
        let r = rx.recv_timeout(timeout);
        self.shared.pending.lock().unwrap().remove(&id);
        match r {
            Ok(v) => Ok(v),
            Err(_) if self.is_closed() => {
                bail!("TV đã ngắt kết nối Cast ({})", self.close_reason().unwrap_or_default())
            }
            Err(_) => Err(anyhow!("TV không trả lời yêu cầu {kind} trong {} s", timeout.as_secs())),
        }
    }

    /// Opens a virtual connection to an app's transport (after LAUNCH).
    pub fn connect_transport(&self, transport_id: &str) {
        self.send(NS_CONNECTION, transport_id, json!({"type": "CONNECT", "origin": {}, "userAgent": "NANOCast"}));
    }

    /// Closes virtual connections and the TLS channel, waiting briefly so queued messages
    /// (STOP) leave first.
    pub fn close(mut self, transports: &[&str]) {
        for t in transports {
            self.send(NS_CONNECTION, t, json!({"type": "CLOSE"}));
        }
        self.send(NS_CONNECTION, RECEIVER_ID, json!({"type": "CLOSE"}));
        // Dropping the sender ends the I/O loop once the queue is written.
        let thread = self.thread.take();
        drop(self);
        if let Some(t) = thread {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

type Tls = StreamOwned<ClientConnection, TcpStream>;

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn io_loop(tls: &mut Tls, out: &Receiver<CastMessage>, events: &Sender<CastMessage>, shared: &Shared) -> Result<()> {
    let mut inbuf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let mut last_ping = Instant::now();
    let mut last_rx = Instant::now();
    loop {
        // Outgoing first; a dropped `Channel` (sender gone) ends the loop after the queue.
        loop {
            match out.try_recv() {
                Ok(m) => tls.write_all(&m.to_frame()).context("ghi tới TV")?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tls.flush()?;
                    tls.conn.send_close_notify();
                    let _ = tls.conn.complete_io(&mut tls.sock);
                    return Ok(());
                }
            }
        }
        if last_ping.elapsed() >= PING_EVERY {
            last_ping = Instant::now();
            let ping = CastMessage::json(SENDER_ID, RECEIVER_ID, NS_HEARTBEAT, r#"{"type":"PING"}"#);
            tls.write_all(&ping.to_frame()).context("ghi tới TV")?;
        }
        if last_rx.elapsed() > DEAD_AFTER {
            bail!("TV không còn trả lời (mất mạng hoặc TV đã tắt)");
        }
        match tls.read(&mut chunk) {
            Ok(0) => bail!("TV đã đóng kết nối"),
            Ok(n) => {
                last_rx = Instant::now();
                inbuf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(e).context("đọc từ TV"),
        }
        while inbuf.len() >= 4 {
            let len = u32::from_be_bytes(inbuf[..4].try_into().unwrap()) as usize;
            if len > MAX_MESSAGE {
                bail!("TV gửi bản tin Cast hỏng ({len} B)");
            }
            if inbuf.len() < 4 + len {
                break;
            }
            let msg = CastMessage::from_body(&inbuf[4..4 + len]);
            inbuf.drain(..4 + len);
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!("bad Cast message: {e:#}");
                    continue;
                }
            };
            route(tls, msg, events, shared)?;
        }
    }
}

fn route(tls: &mut Tls, msg: CastMessage, events: &Sender<CastMessage>, shared: &Shared) -> Result<()> {
    let payload = msg.payload_json();
    let kind = payload.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("").to_string();
    if msg.namespace == NS_HEARTBEAT {
        if kind == "PING" {
            let pong = CastMessage::json(SENDER_ID, &msg.source_id, NS_HEARTBEAT, r#"{"type":"PONG"}"#);
            tls.write_all(&pong.to_frame())?;
        }
        return Ok(());
    }
    debug!("cast <- {} {}: {}", msg.source_id, msg.namespace, msg.payload_utf8.as_deref().unwrap_or("<binary>"));
    if msg.namespace == NS_CONNECTION && kind == "CLOSE" {
        info!("device closed virtual connection {}", msg.source_id);
    }
    if let Some(p) = &payload {
        if let Some(id) = p["requestId"].as_u64().filter(|&id| id != 0) {
            if let Some(tx) = shared.pending.lock().unwrap().remove(&id) {
                let _ = tx.try_send(p.clone());
                return Ok(());
            }
        }
    }
    let _ = events.send(msg);
    Ok(())
}

#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

/// Receiver control on top of a channel: launch / stop apps, volume.
pub struct ReceiverApp {
    pub session_id: String,
    pub transport_id: String,
}

impl Channel {
    /// Launches `app_id` and connects to its transport. Reuses the app when it already runs.
    pub fn launch(&self, app_id: &str) -> Result<ReceiverApp> {
        let status = self.request(NS_RECEIVER, RECEIVER_ID, json!({"type": "LAUNCH", "appId": app_id}), Duration::from_secs(20))?;
        let ty = status["type"].as_str().unwrap_or("");
        if ty == "LAUNCH_ERROR" {
            bail!("TV không mở được ứng dụng Cast {app_id}: {}", status["reason"].as_str().unwrap_or("không rõ lý do"));
        }
        let app = find_app(&status, app_id).or_else(|| {
            // Some devices answer LAUNCH before the app is up; ask again a few times.
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(s) = self.request(NS_RECEIVER, RECEIVER_ID, json!({"type": "GET_STATUS"}), Duration::from_secs(5)) {
                    if let Some(a) = find_app(&s, app_id) {
                        return Some(a);
                    }
                }
            }
            None
        });
        let app = app.ok_or_else(|| anyhow!("TV không chạy ứng dụng Cast {app_id} (TV có thể không hỗ trợ)"))?;
        info!("cast app {app_id} running: session {} transport {}", app.session_id, app.transport_id);
        self.connect_transport(&app.transport_id);
        Ok(app)
    }

    /// Stops a launched app (the TV goes back to its home / ambient screen).
    pub fn stop_app(&self, session_id: &str) {
        self.send(NS_RECEIVER, RECEIVER_ID, json!({"type": "STOP", "sessionId": session_id, "requestId": 9_000_000}));
    }
}

fn find_app(status: &Value, app_id: &str) -> Option<ReceiverApp> {
    let apps = status["status"]["applications"].as_array()?;
    let a = apps.iter().find(|a| a["appId"].as_str() == Some(app_id))?;
    Some(ReceiverApp {
        session_id: a["sessionId"].as_str()?.to_string(),
        transport_id: a["transportId"].as_str()?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_app_in_receiver_status() {
        let s = json!({"type":"RECEIVER_STATUS","requestId":3,"status":{"applications":[
            {"appId":"E8C28D3C","sessionId":"x","transportId":"t-x"},
            {"appId":"CC1AD845","displayName":"Default Media Receiver","sessionId":"abc","transportId":"web-5"}]}});
        let a = find_app(&s, "CC1AD845").unwrap();
        assert_eq!(a.session_id, "abc");
        assert_eq!(a.transport_id, "web-5");
        assert!(find_app(&s, "0F5096E8").is_none());
    }
}
