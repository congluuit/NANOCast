//! Sender side of the control channel: hello/start/stop out, caps/keyframe_request/stats in.
//! Optional by design: if the receiver has no control port the UDP stream still works.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use nanocast_core::control::{self, ControlReader, Message};
use nanocast_core::protocol::PROTOCOL_VERSION;

pub struct ControlLink {
    stream: TcpStream,
    reader: Option<JoinHandle<()>>,
    closing: Arc<AtomicBool>,
    broken: bool,
    pub keyframe_requests: Arc<AtomicUsize>,
}

impl ControlLink {
    /// Connects, sends `hello`, and starts a reader thread that sets `want_keyframe`
    /// whenever the receiver sends `keyframe_request`.
    pub fn open(addr: SocketAddr, name: &str, want_keyframe: Arc<AtomicBool>) -> io::Result<Self> {
        let mut stream = control::connect(addr, Duration::from_secs(1))?;
        control::write_msg(&mut stream, &Message::Hello { ver: PROTOCOL_VERSION.into(), name: name.into() })?;
        let closing = Arc::new(AtomicBool::new(false));
        let keyframe_requests = Arc::new(AtomicUsize::new(0));
        let reader = {
            let mut reader = ControlReader::new(stream.try_clone()?);
            let (closing, requests) = (closing.clone(), keyframe_requests.clone());
            std::thread::spawn(move || loop {
                match reader.read() {
                    Ok(Some(msg)) => handle(msg, &want_keyframe, &requests),
                    Ok(None) => {
                        if !closing.load(Ordering::Relaxed) {
                            println!("[ctrl] receiver closed the control connection");
                        }
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => eprintln!("[ctrl] skipped: {e}"),
                    Err(e) => {
                        if !closing.load(Ordering::Relaxed) {
                            eprintln!("[ctrl] read error: {e}");
                        }
                        break;
                    }
                }
            })
        };
        Ok(Self { stream, reader: Some(reader), closing, broken: false, keyframe_requests })
    }

    /// Sends a message; after the first failure the link goes quiet (video keeps streaming).
    pub fn send(&mut self, msg: &Message) {
        if self.broken {
            return;
        }
        if let Err(e) = control::write_msg(&mut self.stream, msg) {
            eprintln!("warning: control channel lost ({e}); video continues without it");
            self.broken = true;
        }
    }

    /// Sends `stop` and closes the connection.
    pub fn close(mut self) {
        self.send(&Message::Stop);
        self.closing.store(true, Ordering::Relaxed);
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn handle(msg: Message, want_keyframe: &AtomicBool, requests: &AtomicUsize) {
    match msg {
        Message::Caps { ver, name, max_w, max_h, codecs, low_latency } => println!(
            "[ctrl] receiver caps: \"{name}\" v{ver}, max {max_w}x{max_h}, codecs {codecs:?}, low-latency decoder {low_latency}"
        ),
        Message::KeyframeRequest { reason } => {
            want_keyframe.store(true, Ordering::Relaxed);
            let n = requests.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[ctrl] keyframe_request #{n} ({reason})");
        }
        Message::Stats { fps, lost, decode_ms } => {
            println!("[ctrl] receiver stats: {fps:.1} fps, lost {lost} pkts, decode {decode_ms:.1} ms")
        }
        other => eprintln!("[ctrl] ignored unexpected message {other:?}"),
    }
}
