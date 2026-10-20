//! Tiny HTTP server for the live fMP4 stream (`/live.mp4`), std threads only.
//!
//! Each client gets the init segment, then fragments starting at the next video keyframe,
//! with chunked transfer encoding (no length: the stream never ends). A client that falls
//! behind (queue full) skips to the next keyframe instead of growing its delay.
//! `/` serves a test page with a `<video>` element, for checking the stream in Chrome.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result};
use log::{debug, info, warn};

pub const STREAM_PATH: &str = "/live.mp4";
/// Fragments queued per client (~2 s at 60 fps plus audio) before it is resynced.
const CLIENT_QUEUE: usize = 256;

/// A fragment ready to send; `keyframe` = a video fragment starting with an IDR.
#[derive(Clone)]
pub struct Chunk {
    pub data: Arc<Vec<u8>>,
    pub keyframe: bool,
    /// Media time of the fragment (session clock).
    pub pts_us: u64,
}

struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    /// Waiting for a video keyframe before (re)starting the flow.
    waiting: bool,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    init: Option<Arc<Vec<u8>>>,
    clients: Vec<Client>,
}

pub struct LiveServer {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub port: u16,
    /// Clients ever connected / currently connected.
    pub requests: Arc<AtomicU64>,
    pub connected: Arc<AtomicUsize>,
    /// Media time of the keyframe the latest client started at (for the delay estimate).
    pub start_pts_us: AtomicU64,
}

impl LiveServer {
    /// Listens on all interfaces; `port` 0 = any free port.
    pub fn start(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
            .with_context(|| format!("không mở được cổng HTTP {port} trên máy tính"))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let connected = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (state, stop, requests, connected) = (state.clone(), stop.clone(), requests.clone(), connected.clone());
            std::thread::Builder::new().name("cast-http".into()).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((sock, peer)) => {
                            let (state, requests, connected) = (state.clone(), requests.clone(), connected.clone());
                            let _ = std::thread::Builder::new().name("cast-http-client".into()).spawn(move || {
                                if let Err(e) = serve(sock, peer, &state, &requests, &connected) {
                                    debug!("http client {peer}: {e:#}");
                                }
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                        Err(e) => {
                            warn!("http accept: {e}");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })?
        };
        info!("live fMP4 server on port {port}");
        Ok(Self { state, stop, thread: Some(thread), port, requests, connected, start_pts_us: AtomicU64::new(0) })
    }

    /// Sets (or replaces) the init segment; clients connecting from now on get it first.
    pub fn set_init(&self, init: Vec<u8>) {
        self.state.lock().unwrap().init = Some(Arc::new(init));
    }

    pub fn has_init(&self) -> bool {
        self.state.lock().unwrap().init.is_some()
    }

    /// Hands a fragment to every client.
    pub fn push(&self, chunk: Chunk) {
        let mut st = self.state.lock().unwrap();
        st.clients.retain(|c| c.alive.load(Ordering::Relaxed));
        for c in st.clients.iter_mut() {
            if c.waiting {
                if !chunk.keyframe {
                    continue;
                }
                c.waiting = false;
                self.start_pts_us.store(chunk.pts_us, Ordering::Relaxed);
            }
            match c.tx.try_send(chunk.data.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    warn!("http client too slow; skipping to the next keyframe");
                    c.waiting = true;
                }
                Err(TrySendError::Disconnected(_)) => c.alive.store(false, Ordering::Relaxed),
            }
        }
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Disconnecting the queues ends the client threads.
        self.state.lock().unwrap().clients.clear();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const TEST_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>NANOCast live</title>
<style>body{margin:0;background:#000;color:#ccc;font:14px sans-serif}video{width:100vw;height:96vh}</style>
<video id="v" src="/live.mp4" autoplay muted controls></video>
<div id="s"></div>
<script>
const v=document.getElementById('v'),s=document.getElementById('s');
setInterval(()=>{const b=v.buffered;const end=b.length?b.end(b.length-1):0;
s.textContent=`t=${v.currentTime.toFixed(2)} buffered_end=${end.toFixed(2)} lag=${(end-v.currentTime).toFixed(2)}s `+
`${v.videoWidth}x${v.videoHeight} decoded=${v.getVideoPlaybackQuality?v.getVideoPlaybackQuality().totalVideoFrames:'?'} `+
`dropped=${v.getVideoPlaybackQuality?v.getVideoPlaybackQuality().droppedVideoFrames:'?'} state=${v.readyState} err=${v.error?v.error.code+' '+v.error.message:''}`;},500);
</script>"#;

fn serve(sock: TcpStream, peer: SocketAddr, state: &Mutex<State>, requests: &AtomicU64, connected: &AtomicUsize) -> Result<()> {
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    // Headers are not needed (Range is ignored: the answer is always the live stream).
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut sock = sock;
    let path = path.split('?').next().unwrap_or("");
    debug!("http {peer}: {method} {path}");
    match path {
        "/" | "/index.html" => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                TEST_PAGE.len()
            );
            sock.write_all(head.as_bytes())?;
            if method != "HEAD" {
                sock.write_all(TEST_PAGE.as_bytes())?;
            }
            return Ok(());
        }
        STREAM_PATH => {}
        _ => {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nTransfer-Encoding: chunked\r\nCache-Control: no-cache, no-store\r\n\
                Accept-Ranges: none\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n";
    sock.write_all(head.as_bytes())?;
    if method == "HEAD" {
        return Ok(());
    }
    // Wait for the init segment (the first keyframe gives SPS / PPS).
    let (rx, alive): (Receiver<Arc<Vec<u8>>>, Arc<AtomicBool>);
    let init = {
        let mut waited = 0;
        loop {
            if let Some(init) = state.lock().unwrap().init.clone() {
                break init;
            }
            waited += 1;
            if waited > 200 {
                anyhow::bail!("no init segment after 10 s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    {
        let (tx, r) = sync_channel(CLIENT_QUEUE);
        rx = r;
        alive = Arc::new(AtomicBool::new(true));
        state.lock().unwrap().clients.push(Client { tx, waiting: true, alive: alive.clone() });
    }
    requests.fetch_add(1, Ordering::Relaxed);
    connected.fetch_add(1, Ordering::Relaxed);
    info!("http {peer}: streaming {STREAM_PATH}");
    let result = (|| -> Result<()> {
        write_chunk(&mut sock, &init)?;
        while let Ok(data) = rx.recv() {
            write_chunk(&mut sock, &data)?;
        }
        sock.write_all(b"0\r\n\r\n")?;
        Ok(())
    })();
    alive.store(false, Ordering::Relaxed);
    connected.fetch_sub(1, Ordering::Relaxed);
    info!("http {peer}: stream ended ({})", result.as_ref().err().map(|e| e.to_string()).unwrap_or_else(|| "server stopped".into()));
    result
}

fn write_chunk(sock: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(data.len() + 16);
    buf.extend_from_slice(format!("{:X}\r\n", data.len()).as_bytes());
    buf.extend_from_slice(data);
    buf.extend_from_slice(b"\r\n");
    sock.write_all(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn serves_init_then_fragments_from_keyframe() {
        let srv = LiveServer::start(0).unwrap();
        srv.set_init(b"INIT".to_vec());
        let mut c = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        c.write_all(b"GET /live.mp4 HTTP/1.1\r\nHost: x\r\nRange: bytes=0-\r\n\r\n").unwrap();
        // Let the client register before pushing.
        for _ in 0..100 {
            if srv.connected.load(Ordering::Relaxed) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        srv.push(Chunk { data: Arc::new(b"DELTA".to_vec()), keyframe: false, pts_us: 0 });
        srv.push(Chunk { data: Arc::new(b"KEY".to_vec()), keyframe: true, pts_us: 5 });
        srv.push(Chunk { data: Arc::new(b"AUDIO".to_vec()), keyframe: false, pts_us: 6 });
        assert_eq!(srv.start_pts_us.load(Ordering::Relaxed), 5);
        drop(srv);
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        assert!(got.starts_with("HTTP/1.1 200 OK"), "{got}");
        assert!(got.contains("Transfer-Encoding: chunked"));
        let body = got.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.starts_with("4\r\nINIT\r\n3\r\nKEY\r\n5\r\nAUDIO\r\n"), "{body:?}");
        assert!(!body.contains("DELTA"));
    }

    #[test]
    fn test_page_and_404() {
        let srv = LiveServer::start(0).unwrap();
        let get = |path: &str| {
            let mut c = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
            c.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            s
        };
        assert!(get("/").contains("<video"));
        assert!(get("/nope").starts_with("HTTP/1.1 404"));
    }
}
