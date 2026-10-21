//! A small ADB client that talks straight to a TV's `adbd` over TCP ("network debugging",
//! port 5555): connect and authenticate with our own RSA key, push a file, run shell
//! commands. Enough to install the receiver APK without `adb.exe` or a USB cable.
//!
//! Android 11+ "Wireless debugging" answers `STLS`: the connection goes on inside TLS
//! (`adb_wifi`), which works once the TV was paired with its 6-digit code; before that it
//! is reported as [`Fail::NeedsPairing`].

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey};

use crate::adb_wifi;

pub const PORT: u16 = 5555;

const A_CNXN: u32 = 0x4e58_4e43;
const A_AUTH: u32 = 0x4854_5541;
const A_OPEN: u32 = 0x4e45_504f;
const A_OKAY: u32 = 0x5941_4b4f;
const A_CLSE: u32 = 0x4553_4c43;
const A_WRTE: u32 = 0x4554_5257;
const A_STLS: u32 = 0x534c_5453;
const STLS_VERSION: u32 = 0x0100_0000;

const AUTH_TOKEN: u32 = 1;
const AUTH_SIGNATURE: u32 = 2;
const AUTH_RSAPUBLICKEY: u32 = 3;

/// Version with unchecked payload checksums (Android 7+ adbd still accepts the old one).
const VERSION: u32 = 0x0100_0001;
const MAX_DATA: usize = 256 * 1024;
/// Sync `DATA` chunks are at most 64 KiB.
const SYNC_CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// DigestInfo prefix of SHA-1: adbd checks `RSA_verify(NID_sha1, token)`, the 20-byte
/// token standing in for the digest.
const SHA1_PREFIX: [u8; 15] = [0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];

/// Why talking to the TV did not work, in the terms the GUI explains.
#[derive(Debug)]
pub enum Fail {
    /// Nothing answers on the ADB port: debugging is off, wrong address, or another network.
    NoAdb(String),
    /// The user did not press "Allow" on the TV in time (or pressed "Deny").
    NotAllowed,
    /// Android 11+ Wireless debugging (TLS) does not know our key yet: pair with the code.
    NeedsPairing,
    /// Pairing failed: not the code the TV shows (or its dialog was closed meanwhile).
    WrongCode,
    /// Cancelled from the window.
    Cancelled,
    Other(String),
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Fail::NoAdb(e) => write!(f, "no ADB answering at {e}"),
            Fail::NotAllowed => write!(f, "debugging was not allowed on the TV"),
            Fail::NeedsPairing => write!(f, "the TV wants TLS (Wireless debugging) and does not know our key yet: pair with the code"),
            Fail::WrongCode => write!(f, "pairing failed: wrong or expired pairing code"),
            Fail::Cancelled => write!(f, "cancelled"),
            Fail::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Other(format!("ADB: {e}"))
    }
}

/// Our RSA key: the TV remembers it after "Always allow from this computer".
pub struct Key(pub(crate) RsaPrivateKey);

impl Key {
    /// Loads the PKCS#8 PEM at `path`, or makes a new key and saves it there.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if let Ok(pem) = std::fs::read_to_string(path) {
            match RsaPrivateKey::from_pkcs8_pem(&pem) {
                Ok(k) if k.size() == 256 => return Ok(Self(k)),
                Ok(_) => log::warn!("{}: not a 2048-bit key; making a new one", path.display()),
                Err(e) => log::warn!("{}: {e}; making a new one", path.display()),
            }
        }
        // adbd only takes 2048-bit keys with e = 65537.
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).map_err(|e| format!("making the ADB key: {e}"))?;
        let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(|e| format!("encoding the ADB key: {e}"))?;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, pem.as_bytes()) {
            // Still usable; the TV will just ask again next time.
            log::warn!("saving {}: {e}", path.display());
        }
        Ok(Self(key))
    }

    fn sign(&self, token: &[u8]) -> Result<Vec<u8>, Fail> {
        let mut digest = SHA1_PREFIX.to_vec();
        digest.extend_from_slice(token);
        self.0.sign(Pkcs1v15Sign::new_unprefixed(), &digest).map_err(|e| Fail::Other(format!("signing the ADB token: {e}")))
    }

    /// The public key in Android's `adbkey.pub` form: base64 of the `RSAPublicKey` struct
    /// from `android_pubkey.c`, then " name", NUL-terminated.
    pub fn android_public(&self, name: &str) -> Vec<u8> {
        const WORDS: usize = 64;
        let n = self.0.n();
        let n0 = le_words(n, 1)[0];
        // -1 / n mod 2^32 by Newton's iteration (n is odd).
        let mut inv = n0;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n0.wrapping_mul(inv)));
        }
        let rr = (BigUint::from(1u64) << (WORDS * 64)) % n;
        let e: u32 = le_words(self.0.e(), 1)[0];
        let mut raw = Vec::with_capacity(4 * (3 + 2 * WORDS));
        raw.extend_from_slice(&(WORDS as u32).to_le_bytes());
        raw.extend_from_slice(&inv.wrapping_neg().to_le_bytes());
        for w in le_words(n, WORDS).into_iter().chain(le_words(&rr, WORDS)) {
            raw.extend_from_slice(&w.to_le_bytes());
        }
        raw.extend_from_slice(&e.to_le_bytes());
        let mut out = base64(&raw).into_bytes();
        out.push(b' ');
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out
    }
}

/// `v` as `count` little-endian 32-bit words.
fn le_words(v: &BigUint, count: usize) -> Vec<u32> {
    let mut bytes = v.to_bytes_le();
    bytes.resize(count * 4, 0);
    bytes.as_chunks::<4>().0.iter().take(count).map(|c| u32::from_le_bytes(*c)).collect()
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

struct Packet {
    cmd: u32,
    arg0: u32,
    arg1: u32,
    data: Vec<u8>,
}

/// Plain TCP, or TLS after `STLS` (Wireless debugging).
enum Link {
    Plain(TcpStream),
    Tls(Box<adb_wifi::Tls>),
}

impl Link {
    fn tcp(&self) -> &TcpStream {
        match self {
            Link::Plain(s) => s,
            Link::Tls(t) => &t.sock,
        }
    }
}

impl Read for Link {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Plain(s) => s.read(buf),
            Link::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Link {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Link::Plain(s) => s.write(buf),
            Link::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Link::Plain(s) => s.flush(),
            Link::Tls(t) => t.flush(),
        }
    }
}

/// An authenticated connection to one TV. Streams are used one at a time.
pub struct Device {
    link: Link,
    max_data: usize,
    next_id: u32,
    /// `device::ro.product.name=…;ro.product.model=…;…`
    pub banner: String,
}

impl Device {
    /// Connects and authenticates. When the TV does not know our key yet, `on_prompt` runs
    /// once ("press Allow on the TV") and we wait up to `allow_for` for the answer, or until
    /// `cancel` is set. A TV that wants TLS gets it, with our key in the client certificate.
    pub fn connect(addr: SocketAddr, key: &Key, name: &str, allow_for: Duration, cancel: &AtomicBool, mut on_prompt: impl FnMut()) -> Result<Self, Fail> {
        let sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Fail::NoAdb(format!("{addr}: {e}")))?;
        let _ = sock.set_nodelay(true);
        sock.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut dev = Self { link: Link::Plain(sock), max_data: MAX_DATA, next_id: 1, banner: String::new() };
        dev.send(A_CNXN, VERSION, MAX_DATA as u32, b"host::features=cmd\0")?;
        let (mut signed, mut prompted) = (false, false);
        let mut deadline = Instant::now() + IO_TIMEOUT;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::Cancelled);
            }
            // Short reads so Cancel works while the TV shows its dialog.
            let p = match dev.recv_until(deadline, cancel) {
                Ok(p) => p,
                // TLS 1.3: a TV that does not know our certificate hangs up after the handshake.
                Err(Fail::Other(e)) if matches!(dev.link, Link::Tls(_)) => {
                    log::info!("ADB over TLS at {addr}: {e}");
                    return Err(Fail::NeedsPairing);
                }
                Err(Fail::Other(_)) if prompted => return Err(Fail::NotAllowed),
                Err(Fail::Other(e)) if !signed => return Err(Fail::NoAdb(e)),
                Err(e) => return Err(e),
            };
            match p.cmd {
                A_CNXN => {
                    dev.max_data = (p.arg1 as usize).clamp(1024, MAX_DATA);
                    dev.banner = String::from_utf8_lossy(&p.data).trim_end_matches('\0').to_string();
                    dev.link.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
                    return Ok(dev);
                }
                A_STLS if matches!(dev.link, Link::Plain(_)) => {
                    dev.send(A_STLS, STLS_VERSION, 0, &[])?;
                    let sock = dev.link.tcp().try_clone()?;
                    dev.link = match adb_wifi::tls_client(sock, key, addr.ip()) {
                        Ok(tls) => Link::Tls(Box::new(tls)),
                        Err(e) => {
                            log::info!("TLS handshake with {addr}: {e}");
                            return Err(Fail::NeedsPairing);
                        }
                    };
                    deadline = Instant::now() + IO_TIMEOUT;
                }
                A_AUTH if p.arg0 == AUTH_TOKEN => {
                    if !signed {
                        signed = true;
                        let sig = key.sign(&p.data)?;
                        dev.send(A_AUTH, AUTH_SIGNATURE, 0, &sig)?;
                    } else if !prompted {
                        prompted = true;
                        dev.send(A_AUTH, AUTH_RSAPUBLICKEY, 0, &key.android_public(name))?;
                        on_prompt();
                        deadline = Instant::now() + allow_for;
                    } else {
                        // A new token after the dialog: it was dismissed.
                        return Err(Fail::NotAllowed);
                    }
                }
                _ => {}
            }
        }
    }

    /// The TV model from the connect banner, if it says.
    pub fn model(&self) -> Option<String> {
        self.banner.split(';').find_map(|kv| kv.strip_prefix("ro.product.model=")).map(str::to_string).filter(|m| !m.is_empty())
    }

    /// Runs `cmd` in `shell:` and returns its output once it exits.
    pub fn shell(&mut self, cmd: &str, timeout: Duration) -> Result<String, Fail> {
        let (local, remote) = self.open(&format!("shell:{cmd}"))?;
        let out = self.read_to_close(local, remote, Instant::now() + timeout)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Sends a shell command that cuts the connection (turning ADB off): only the sending
    /// has to work.
    pub fn shell_last(mut self, cmd: &str) -> Result<(), Fail> {
        let (local, remote) = self.open(&format!("shell:{cmd}"))?;
        let _ = self.read_to_close(local, remote, Instant::now() + Duration::from_secs(5));
        Ok(())
    }

    /// Writes `data` to `remote_path` (mode 0644) with the sync protocol; `progress` gets
    /// the bytes sent so far.
    pub fn push(&mut self, data: &[u8], remote_path: &str, cancel: &AtomicBool, mut progress: impl FnMut(usize)) -> Result<(), Fail> {
        let (local, remote) = self.open("sync:")?;
        let spec = format!("{remote_path},{}", 0o100644);
        let mut buf = sync_header(b"SEND", spec.len() as u32);
        buf.extend_from_slice(spec.as_bytes());
        let mut sent = 0;
        for chunk in data.chunks(SYNC_CHUNK) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::Cancelled);
            }
            buf.extend_from_slice(&sync_header(b"DATA", chunk.len() as u32));
            buf.extend_from_slice(chunk);
            self.write(local, remote, &buf)?;
            buf.clear();
            sent += chunk.len();
            progress(sent);
        }
        let mtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
        self.write(local, remote, &sync_header(b"DONE", mtime))?;
        let reply = self.read_exact_stream(local, remote, 8)?;
        if &reply[..4] != b"OKAY" {
            let len = u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]) as usize;
            let msg = self.read_exact_stream(local, remote, len.min(4096)).unwrap_or_default();
            return Err(Fail::Other(format!("copying the APK to the TV: {}", String::from_utf8_lossy(&msg))));
        }
        self.write(local, remote, &sync_header(b"QUIT", 0))?;
        let _ = self.read_to_close(local, remote, Instant::now() + Duration::from_secs(3));
        Ok(())
    }

    // ---------- streams ----------

    fn open(&mut self, service: &str) -> Result<(u32, u32), Fail> {
        let local = self.next_id;
        self.next_id += 1;
        let mut dest = service.as_bytes().to_vec();
        dest.push(0);
        self.send(A_OPEN, local, 0, &dest)?;
        loop {
            let p = self.recv()?;
            if p.arg1 != local {
                continue;
            }
            match p.cmd {
                A_OKAY => return Ok((local, p.arg0)),
                A_CLSE => return Err(Fail::Other(format!("the TV refused `{}`", service.split(':').next().unwrap_or(service)))),
                _ => {}
            }
        }
    }

    /// Sends `data` in packets of at most `max_data`, each acknowledged before the next.
    fn write(&mut self, local: u32, remote: u32, data: &[u8]) -> Result<(), Fail> {
        for chunk in data.chunks(self.max_data) {
            self.send(A_WRTE, local, remote, chunk)?;
            loop {
                let p = self.recv()?;
                if p.arg1 != local {
                    continue;
                }
                match p.cmd {
                    A_OKAY => break,
                    A_CLSE => return Err(Fail::Other("the TV closed the stream".into())),
                    // An early reply (a sync FAIL): keep it for the reader.
                    A_WRTE => {
                        self.send(A_OKAY, local, remote, &[])?;
                        return Err(Fail::Other(format!("the TV answered early: {}", String::from_utf8_lossy(&p.data))));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn read_exact_stream(&mut self, local: u32, remote: u32, n: usize) -> Result<Vec<u8>, Fail> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let p = self.recv()?;
            if p.arg1 != local {
                continue;
            }
            match p.cmd {
                A_WRTE => {
                    self.send(A_OKAY, local, remote, &[])?;
                    out.extend_from_slice(&p.data);
                }
                A_CLSE => break,
                _ => {}
            }
        }
        if out.len() < n {
            return Err(Fail::Other("the TV closed the stream early".into()));
        }
        Ok(out)
    }

    fn read_to_close(&mut self, local: u32, remote: u32, deadline: Instant) -> Result<Vec<u8>, Fail> {
        let never = AtomicBool::new(false);
        let mut out = Vec::new();
        loop {
            let p = self.recv_until(deadline, &never)?;
            if p.arg1 != local {
                continue;
            }
            match p.cmd {
                A_WRTE => {
                    self.send(A_OKAY, local, remote, &[])?;
                    out.extend_from_slice(&p.data);
                }
                A_CLSE => {
                    let _ = self.send(A_CLSE, local, remote, &[]);
                    return Ok(out);
                }
                _ => {}
            }
        }
    }

    // ---------- packets ----------

    fn send(&mut self, cmd: u32, arg0: u32, arg1: u32, data: &[u8]) -> Result<(), Fail> {
        let mut msg = Vec::with_capacity(24 + data.len());
        let sum: u32 = data.iter().map(|&b| u32::from(b)).sum();
        for v in [cmd, arg0, arg1, data.len() as u32, sum, !cmd] {
            msg.extend_from_slice(&v.to_le_bytes());
        }
        msg.extend_from_slice(data);
        self.link.write_all(&msg)?;
        // TLS only sends (and reports errors) on flush.
        self.link.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Packet, Fail> {
        let never = AtomicBool::new(false);
        self.recv_until(Instant::now() + IO_TIMEOUT, &never)
    }

    /// One packet, polling every 250 ms so `deadline` and `cancel` are honoured.
    fn recv_until(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<Packet, Fail> {
        let mut head = [0u8; 24];
        let mut got = 0;
        while got < head.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::Cancelled);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Fail::Other("the TV did not answer in time".into()));
            }
            self.link.tcp().set_read_timeout(Some(left.min(Duration::from_millis(250))))?;
            match self.link.read(&mut head[got..]) {
                Ok(0) => return Err(Fail::Other("the TV closed the connection".into())),
                Ok(n) => got += n,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                Err(e) => return Err(Fail::Other(format!("ADB: {e}"))),
            }
        }
        let word = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let (cmd, len, magic) = (word(0), word(12) as usize, word(20));
        if magic != !cmd || len > 1024 * 1024 {
            return Err(Fail::Other(format!("not an ADB answer (header {:08x})", cmd)));
        }
        self.link.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
        let mut data = vec![0u8; len];
        self.link.read_exact(&mut data)?;
        Ok(Packet { cmd, arg0: word(4), arg1: word(8), data })
    }
}

fn sync_header(id: &[u8; 4], n: u32) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&n.to_le_bytes());
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn little_endian_words() {
        let v = BigUint::from(0x1_0000_0002u64);
        assert_eq!(le_words(&v, 3), vec![2, 1, 0]);
    }

    pub fn test_key() -> &'static Key {
        static KEY: std::sync::OnceLock<Key> = std::sync::OnceLock::new();
        KEY.get_or_init(|| Key(RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap()))
    }

    /// The TV's own key (its TLS certificate).
    pub fn other_key() -> &'static Key {
        static KEY: std::sync::OnceLock<Key> = std::sync::OnceLock::new();
        KEY.get_or_init(|| Key(RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap()))
    }

    /// A fake adbd on localhost running `script` on the first connection.
    pub fn fake_tv(script: impl FnOnce(&mut TcpStream) + Send + 'static) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let tv = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            script(&mut s);
        });
        (addr, tv)
    }

    fn put(s: &mut impl Write, cmd: u32, arg0: u32, arg1: u32, data: &[u8]) {
        let mut m = Vec::new();
        for v in [cmd, arg0, arg1, data.len() as u32, 0, !cmd] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m.extend_from_slice(data);
        s.write_all(&m).unwrap();
        s.flush().unwrap();
    }

    fn take(s: &mut impl Read) -> Packet {
        let mut h = [0u8; 24];
        s.read_exact(&mut h).unwrap();
        let w = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let mut data = vec![0u8; w(12) as usize];
        s.read_exact(&mut data).unwrap();
        Packet { cmd: w(0), arg0: w(4), arg1: w(8), data }
    }

    fn connect(addr: SocketAddr, allow_for: Duration, prompts: &mut u32) -> Result<Device, Fail> {
        Device::connect(addr, test_key(), "test@pc", allow_for, &AtomicBool::new(false), || *prompts += 1)
    }

    /// Unknown key: signature refused, public key sent, "Allow" pressed, then a shell command.
    #[test]
    fn allow_on_tv_then_shell() {
        let (addr, tv) = fake_tv(|s| {
            assert_eq!(take(s).cmd, A_CNXN);
            let token = [7u8; 20];
            put(s, A_AUTH, AUTH_TOKEN, 0, &token);
            let sig = take(s);
            assert_eq!((sig.cmd, sig.arg0), (A_AUTH, AUTH_SIGNATURE));
            let digest = [&SHA1_PREFIX[..], &token].concat();
            test_key().0.to_public_key().verify(Pkcs1v15Sign::new_unprefixed(), &digest, &sig.data).unwrap();
            put(s, A_AUTH, AUTH_TOKEN, 0, &[8u8; 20]);
            let public = take(s);
            assert_eq!((public.cmd, public.arg0), (A_AUTH, AUTH_RSAPUBLICKEY));
            assert!(public.data.ends_with(b" test@pc\0"));
            std::thread::sleep(Duration::from_millis(300));
            put(s, A_CNXN, VERSION, 4096, b"device::ro.product.name=x;ro.product.model=Mock TV;\0");
            let open = take(s);
            assert_eq!((open.cmd, open.data.as_slice()), (A_OPEN, &b"shell:echo hi\0"[..]));
            put(s, A_OKAY, 9, open.arg0, &[]);
            put(s, A_WRTE, 9, open.arg0, b"hi\n");
            assert_eq!(take(s).cmd, A_OKAY);
            put(s, A_CLSE, 9, open.arg0, &[]);
            assert_eq!(take(s).cmd, A_CLSE);
        });
        let mut prompts = 0;
        let mut dev = connect(addr, Duration::from_secs(5), &mut prompts).unwrap();
        assert_eq!(prompts, 1);
        assert_eq!(dev.max_data, 4096);
        assert_eq!(dev.model().as_deref(), Some("Mock TV"));
        assert_eq!(dev.shell("echo hi", Duration::from_secs(5)).unwrap(), "hi\n");
        tv.join().unwrap();
    }

    #[test]
    fn nobody_presses_allow() {
        let (addr, _tv) = fake_tv(|s| {
            take(s);
            put(s, A_AUTH, AUTH_TOKEN, 0, &[1u8; 20]);
            take(s);
            put(s, A_AUTH, AUTH_TOKEN, 0, &[2u8; 20]);
            take(s);
            std::thread::sleep(Duration::from_secs(2));
        });
        let mut prompts = 0;
        assert!(matches!(connect(addr, Duration::from_millis(500), &mut prompts), Err(Fail::NotAllowed)));
        assert_eq!(prompts, 1);
    }

    /// A Wireless debugging adbd: `STLS`, then TLS letting in only `known` keys, then
    /// `CNXN` and `script` inside TLS. `None` to the script when the TLS handshake failed.
    pub fn fake_tls_tv(known: Vec<Vec<u8>>, script: impl FnOnce(Option<&mut adb_wifi::fake::TlsServer>) + Send + 'static) -> (SocketAddr, std::thread::JoinHandle<()>) {
        fake_tv(move |s| {
            assert_eq!(take(s).cmd, A_CNXN);
            put(s, A_STLS, STLS_VERSION, 0, &[]);
            let stls = take(s);
            assert_eq!((stls.cmd, stls.arg0), (A_STLS, STLS_VERSION));
            match adb_wifi::fake::tls_server(s.try_clone().unwrap(), other_key(), known) {
                Ok(mut tls) => {
                    put(&mut tls, A_CNXN, VERSION, MAX_DATA as u32, b"device::ro.product.name=tv;ro.product.model=TLS TV;\0");
                    script(Some(&mut tls));
                }
                Err(_) => script(None),
            }
        })
    }

    /// Paired already: TLS with our certificate, then the usual streams inside it.
    #[test]
    fn wireless_debugging_tls() {
        let (addr, tv) = fake_tls_tv(vec![adb_wifi::fake::spki(test_key())], |tls| {
            let s = tls.expect("our key is known");
            let open = take(s);
            assert_eq!(open.data, b"shell:echo tls\0");
            put(s, A_OKAY, 3, open.arg0, &[]);
            put(s, A_WRTE, 3, open.arg0, b"tls\n");
            assert_eq!(take(s).cmd, A_OKAY);
            put(s, A_CLSE, 3, open.arg0, &[]);
            assert_eq!(take(s).cmd, A_CLSE);
        });
        let mut prompts = 0;
        let mut dev = connect(addr, Duration::from_secs(1), &mut prompts).unwrap();
        assert_eq!(prompts, 0);
        assert_eq!(dev.model().as_deref(), Some("TLS TV"));
        assert_eq!(dev.shell("echo tls", Duration::from_secs(5)).unwrap(), "tls\n");
        tv.join().unwrap();
    }

    /// Not paired yet: the TV refuses our certificate after the handshake.
    #[test]
    fn wireless_debugging_needs_pairing() {
        let (addr, tv) = fake_tls_tv(vec![adb_wifi::fake::spki(other_key())], |tls| assert!(tls.is_none()));
        assert!(matches!(connect(addr, Duration::from_secs(1), &mut 0), Err(Fail::NeedsPairing)));
        tv.join().unwrap();
    }

    /// An adbd that says `STLS` but then speaks no TLS.
    #[test]
    fn wireless_debugging_broken_tls() {
        let (addr, _tv) = fake_tv(|s| {
            take(s);
            put(s, A_STLS, STLS_VERSION, 0, &[]);
            std::thread::sleep(Duration::from_millis(200));
        });
        assert!(matches!(connect(addr, Duration::from_secs(1), &mut 0), Err(Fail::NeedsPairing)));
    }

    /// Pairing with the code, then the same key gets in over TLS.
    #[test]
    fn pair_then_connect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pair_at = listener.local_addr().unwrap();
        let pairing = std::thread::spawn(move || adb_wifi::fake::serve_pairing(listener.accept().unwrap().0, other_key(), "135790", "adb-TV"));
        assert_eq!(adb_wifi::pair(pair_at, test_key(), "test@pc", "135790").unwrap(), "adb-TV");
        assert!(pairing.join().unwrap().unwrap().starts_with(std::str::from_utf8(&test_key().android_public("test@pc")[..20]).unwrap()));
        let (addr, tv) = fake_tls_tv(vec![adb_wifi::fake::spki(test_key())], |tls| assert!(tls.is_some()));
        assert!(connect(addr, Duration::from_secs(1), &mut 0).is_ok());
        tv.join().unwrap();
    }

    #[test]
    fn debugging_off() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(matches!(connect(addr, Duration::from_secs(1), &mut 0), Err(Fail::NoAdb(_))));
    }

    /// Several sync DATA chunks, split into 4 KiB packets (an old TV's max_data).
    #[test]
    fn push_file() {
        let file: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let expected = file.clone();
        let (addr, tv) = fake_tv(move |s| {
            take(s);
            put(s, A_CNXN, VERSION, 4096, b"device::\0");
            let open = take(s);
            assert_eq!(open.data, b"sync:\0");
            let local = open.arg0;
            put(s, A_OKAY, 5, local, &[]);
            let mut stream = Vec::new();
            loop {
                let p = take(s);
                assert_eq!(p.cmd, A_WRTE);
                assert!(p.data.len() <= 4096);
                stream.extend_from_slice(&p.data);
                put(s, A_OKAY, 5, local, &[]);
                if stream.len() >= 8 && &stream[stream.len() - 8..stream.len() - 4] == b"DONE" {
                    break;
                }
            }
            // SEND <spec>, then DATA chunks, then DONE.
            let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
            assert_eq!(&stream[..4], b"SEND");
            let spec_len = word(&stream[4..]);
            assert_eq!(&stream[8..8 + spec_len], b"/data/local/tmp/x.apk,33188");
            let (mut at, mut content) = (8 + spec_len, Vec::new());
            while &stream[at..at + 4] == b"DATA" {
                let n = word(&stream[at + 4..]);
                assert!(n <= SYNC_CHUNK);
                content.extend_from_slice(&stream[at + 8..at + 8 + n]);
                at += 8 + n;
            }
            assert_eq!(content, expected);
            put(s, A_WRTE, 5, local, b"OKAY\0\0\0\0");
            assert_eq!(take(s).cmd, A_OKAY);
            let quit = take(s);
            assert_eq!(&quit.data[..4], b"QUIT");
            put(s, A_OKAY, 5, local, &[]);
            put(s, A_CLSE, 5, local, &[]);
            take(s);
        });
        let mut dev = connect(addr, Duration::from_secs(1), &mut 0).unwrap();
        let mut sent = 0;
        dev.push(&file, "/data/local/tmp/x.apk", &AtomicBool::new(false), |n| sent = n).unwrap();
        assert_eq!(sent, file.len());
        tv.join().unwrap();
    }

    /// Our `adbkey.pub` encoding matches the one `adb` wrote for the same key.
    #[test]
    #[ignore = "needs ~/.android/adbkey and adbkey.pub from the Android SDK"]
    fn public_key_matches_adb() {
        let home = directories::BaseDirs::new().unwrap().home_dir().join(".android");
        let pem = std::fs::read_to_string(home.join("adbkey")).unwrap();
        let key = Key(RsaPrivateKey::from_pkcs8_pem(&pem).unwrap());
        let expected = std::fs::read_to_string(home.join("adbkey.pub")).unwrap();
        let ours = key.android_public("x");
        let ours = String::from_utf8_lossy(&ours);
        assert_eq!(ours.split(' ').next(), expected.split(' ').next());
    }
}
