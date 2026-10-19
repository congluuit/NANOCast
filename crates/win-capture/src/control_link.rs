//! Sender side of the control channel: hello/start/audio/pause/blank/stop out,
//! caps/error/keyframe_request/stats in.
//! Optional by design: if the receiver has no control port the UDP stream still works.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{info, warn};
use nanocast_core::control::{self, ControlReader, Message};
use nanocast_core::protocol::PROTOCOL_VERSION;

/// What the receiver told us, shared with whoever shows it.
#[derive(Clone, Debug, Default)]
pub struct ReceiverReport {
    /// From `caps`.
    pub name: Option<String>,
    pub max_size: Option<(u32, u32)>,
    pub low_latency: Option<bool>,
    /// `caps.codecs` ("h264", "hevc").
    pub codecs: Option<Vec<String>>,
    /// `error.code` from the receiver, e.g. `bad_pin` (it closes the connection after it).
    pub error: Option<String>,
    /// From the latest `stats` (one per second).
    pub stats: Option<TvStats>,
    /// Packets lost, summed over every `stats` received.
    pub lost_total: u64,
    /// Number of `stats` received (lets the adaptive bitrate see each new report once).
    pub stats_count: u64,
    /// `false` once the receiver closed the connection or it failed.
    pub connected: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TvStats {
    pub fps: f64,
    pub lost: u64,
    pub decode_ms: f64,
}

pub struct ControlLink {
    stream: TcpStream,
    reader: Option<JoinHandle<()>>,
    closing: Arc<AtomicBool>,
    broken: bool,
    pub keyframe_requests: Arc<AtomicUsize>,
    pub report: Arc<Mutex<ReceiverReport>>,
}

impl ControlLink {
    /// Connects (timeout 1 s), sends `hello` (with the TV's 4-digit code if given), and
    /// starts a reader thread that sets `want_keyframe` whenever the receiver sends
    /// `keyframe_request`.
    pub fn open(addr: SocketAddr, name: &str, pin: Option<&str>, want_keyframe: Arc<AtomicBool>) -> io::Result<Self> {
        let mut stream = control::connect(addr, Duration::from_secs(1))?;
        let hello = Message::Hello { ver: PROTOCOL_VERSION.into(), name: name.into(), pin: pin.map(str::to_string) };
        control::write_msg(&mut stream, &hello)?;
        let closing = Arc::new(AtomicBool::new(false));
        let keyframe_requests = Arc::new(AtomicUsize::new(0));
        let report = Arc::new(Mutex::new(ReceiverReport { connected: true, ..Default::default() }));
        let reader = {
            let mut reader = ControlReader::new(stream.try_clone()?);
            let (closing, requests, report) = (closing.clone(), keyframe_requests.clone(), report.clone());
            std::thread::Builder::new().name("ctrl-read".into()).spawn(move || {
                loop {
                    match reader.read() {
                        Ok(Some(msg)) => handle(msg, &want_keyframe, &requests, &report),
                        Ok(None) => {
                            if !closing.load(Ordering::Relaxed) {
                                info!("[ctrl] receiver closed the control connection");
                            }
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::InvalidData => warn!("[ctrl] skipped: {e}"),
                        Err(e) => {
                            if !closing.load(Ordering::Relaxed) {
                                warn!("[ctrl] read error: {e}");
                            }
                            break;
                        }
                    }
                }
                report.lock().unwrap().connected = false;
            })?
        };
        Ok(Self { stream, reader: Some(reader), closing, broken: false, keyframe_requests, report })
    }

    /// Sends a message; after the first failure the link goes quiet (video keeps streaming).
    pub fn send(&mut self, msg: &Message) {
        if self.broken {
            return;
        }
        if let Err(e) = control::write_msg(&mut self.stream, msg) {
            warn!("control channel lost ({e}); video continues without it");
            self.broken = true;
            self.report.lock().unwrap().connected = false;
        }
    }

    /// Sends `stop` and closes the connection (same as dropping it).
    pub fn close(self) {}
}

impl Drop for ControlLink {
    fn drop(&mut self) {
        self.send(&Message::Stop);
        self.closing.store(true, Ordering::Relaxed);
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn handle(msg: Message, want_keyframe: &AtomicBool, requests: &AtomicUsize, report: &Mutex<ReceiverReport>) {
    match msg {
        Message::Caps { ver, name, max_w, max_h, codecs, low_latency } => {
            info!("[ctrl] receiver caps: \"{name}\" v{ver}, max {max_w}x{max_h}, codecs {codecs:?}, low-latency decoder {low_latency}");
            let mut r = report.lock().unwrap();
            r.name = Some(name);
            r.max_size = (max_w > 0 && max_h > 0).then_some((max_w, max_h));
            r.low_latency = Some(low_latency);
            r.codecs = Some(codecs);
        }
        Message::Error { code, message } => {
            warn!("[ctrl] receiver refused: {code} {message}");
            report.lock().unwrap().error = Some(code);
        }
        Message::KeyframeRequest { reason } => {
            want_keyframe.store(true, Ordering::Relaxed);
            let n = requests.fetch_add(1, Ordering::Relaxed) + 1;
            info!("[ctrl] keyframe_request #{n} ({reason})");
        }
        Message::Stats { fps, lost, decode_ms } => {
            log::debug!("[ctrl] receiver stats: {fps:.1} fps, lost {lost} pkts, decode {decode_ms:.1} ms");
            let mut r = report.lock().unwrap();
            r.stats = Some(TvStats { fps, lost, decode_ms });
            r.lost_total += lost;
            r.stats_count += 1;
        }
        other => warn!("[ctrl] ignored unexpected message {other:?}"),
    }
}
