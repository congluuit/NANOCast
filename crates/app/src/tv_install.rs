//! "Install the app on this TV" over Wi-Fi, without a USB stick: get the receiver APK of
//! this version, connect to the TV's ADB (network debugging on port 5555, or Android 11+
//! Wireless debugging: pair with the code once, then TLS on the port the TV announces),
//! wait for "Allow" on the TV, install, open the app, then turn ADB off again. Runs on its
//! own thread; the window polls [`Job::state`].

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::adb::{self, Device, Fail};
use crate::adb_wifi;

const PACKAGE: &str = "com.redharegames.nanocast.receiver";
const REMOTE_APK: &str = "/data/local/tmp/nanocast-receiver.apk";
/// Time to walk to the TV and press "Allow".
pub const ALLOW_FOR: Duration = Duration::from_secs(90);
/// `pm install` waits while Play Protect asks on the TV.
const INSTALL_FOR: Duration = Duration::from_secs(180);
const MAX_APK: u64 = 64 * 1024 * 1024;
/// How long to look for the TV's Wireless debugging ports over mDNS.
const PAIRING_LOOKUP: Duration = Duration::from_secs(6);
const CONNECT_LOOKUP: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    #[default]
    Download,
    Connect,
    Allow,
    Install,
    Open,
    AdbOff,
}

impl Step {
    pub const ALL: [Step; 6] = [Step::Download, Step::Connect, Step::Allow, Step::Install, Step::Open, Step::AdbOff];
}

/// What went wrong, for the words the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The APK could not be downloaded.
    NoInternet,
    /// Nothing answers on port 5555, and no Wireless debugging port is announced.
    NoAdb,
    /// "Allow" was not pressed on the TV.
    NotAllowed,
    /// Android 11+ Wireless debugging is on but has not paired with us: ask for the code.
    NeedsPairing,
    /// Pairing refused: wrong or expired code.
    WrongCode,
    /// A code was typed but the TV's pairing dialog was not found (mDNS): ask for its port.
    NoPairPort,
    /// Paired, but the Wireless debugging port was not found: ask for it.
    NoConnectPort,
    /// Not enough space on the TV.
    LowSpace,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub problem: Problem,
    pub step: Step,
    /// Technical text (Details).
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub step: Step,
    /// Download / copy progress, 0..=1.
    pub fraction: Option<f32>,
    /// `Ok(adb_turned_off)` once finished.
    pub result: Option<Result<bool, Failure>>,
}

/// What the user typed for Android 11+ Wireless debugging; all empty at first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wireless {
    /// The 6-digit code from "Pair device with pairing code": pair before connecting.
    pub code: Option<String>,
    /// Address of that pairing dialog when mDNS does not find it.
    pub pair_at: Option<SocketAddr>,
    /// "IP address & Port" of the Wireless debugging screen when mDNS does not find it.
    pub connect_at: Option<SocketAddr>,
}

/// "192.168.1.20:37123", or just "37123" on the TV's own address.
/// The TV's IP address as people type it: "192,168,1,50" (number pad), spaces, a scheme,
/// a port or a path after it, leading zeros ("192.168.001.050") are all fine.
pub fn parse_tv_ip(text: &str) -> Option<Ipv4Addr> {
    let text = text.trim();
    let text = text.split_once("://").map_or(text, |(_, rest)| rest);
    let text: String = text.chars().filter(|c| !c.is_whitespace()).map(|c| if matches!(c, ',' | '，' | '。') { '.' } else { c }).collect();
    let host = text.split(['/', ':']).next()?;
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 || parts.iter().any(|p| p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let mut octets = [0u8; 4];
    for (o, p) in octets.iter_mut().zip(&parts) {
        *o = p.parse().ok()?;
    }
    let ip = Ipv4Addr::from(octets);
    (!ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()).then_some(ip)
}

pub fn parse_address(text: &str, tv: Ipv4Addr) -> Option<SocketAddr> {
    let text = text.trim();
    text.parse().ok().or_else(|| Some(SocketAddr::new(tv.into(), text.parse::<u16>().ok().filter(|&p| p != 0)?)))
}

pub struct Job {
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    /// `host` names this computer in the key the TV remembers.
    pub fn start(addr: Ipv4Addr, apk_url: String, host: String, wireless: Wireless) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (st, c) = (state.clone(), cancel.clone());
        let spawned = std::thread::Builder::new().name("tv-install".into()).spawn(move || {
            let result = run(addr, &apk_url, &host, &wireless, &st, &c);
            match &result {
                Ok(off) => log::info!("receiver installed on {addr} (ADB turned off: {off})"),
                Err(f) => log::warn!("installing on {addr}: {:?} at {:?}: {}", f.problem, f.step, f.detail),
            }
            st.lock().unwrap().result = Some(result);
        });
        if let Err(e) = spawned {
            let detail = format!("install thread: {e}");
            state.lock().unwrap().result = Some(Err(Failure { problem: Problem::Failed, step: Step::Download, detail }));
        }
        Self { state, cancel }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn running(&self) -> bool {
        self.state.lock().unwrap().result.is_none()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn from_adb(step: Step, e: Fail) -> Failure {
    let problem = match e {
        Fail::NoAdb(_) => Problem::NoAdb,
        Fail::NotAllowed => Problem::NotAllowed,
        Fail::NeedsPairing => Problem::NeedsPairing,
        Fail::WrongCode => Problem::WrongCode,
        Fail::Cancelled | Fail::Other(_) => Problem::Failed,
    };
    Failure { problem, step, detail: e.to_string() }
}

fn run(addr: Ipv4Addr, apk_url: &str, host: &str, wireless: &Wireless, st: &Mutex<State>, cancel: &AtomicBool) -> Result<bool, Failure> {
    let set = |step: Step, fraction: Option<f32>| {
        let mut s = st.lock().unwrap();
        s.step = step;
        s.fraction = fraction;
    };
    let fail = |step: Step, problem: Problem, detail: String| Failure { problem, step, detail };

    set(Step::Download, None);
    let apk = get_apk(apk_url, cancel, |f| set(Step::Download, Some(f))).map_err(|e| fail(Step::Download, Problem::NoInternet, e))?;
    let key = adb::Key::load_or_create(&data_dir().join("adbkey.pem")).map_err(|e| fail(Step::Connect, Problem::Failed, e))?;

    set(Step::Connect, None);
    let mut dev = connect(addr, &key, host, wireless, cancel, || set(Step::Allow, None)).map_err(|(problem, e)| {
        let mut f = from_adb(st.lock().unwrap().step, e);
        f.problem = problem.unwrap_or(f.problem);
        f
    })?;
    log::info!("ADB connected to {addr} ({})", dev.model().unwrap_or_default());

    // From here on ADB is open on the TV: close it again whatever happens.
    let installed = install(&mut dev, &apk, cancel, &set).map_err(|(step, e)| from_adb(step, e)).and_then(|r| r);
    if installed.is_ok() {
        set(Step::Open, None);
        match dev.shell(&format!("am start -n {PACKAGE}/.MainActivity"), Duration::from_secs(20)) {
            Ok(out) if out.contains("Error") => log::warn!("opening the app on the TV: {}", out.trim()),
            Ok(_) => {}
            Err(e) => log::warn!("opening the app on the TV: {e}"),
        }
    }
    set(Step::AdbOff, None);
    // Turning ADB off kills the connection; Android 11+ also has a separate Wi-Fi switch.
    let off = match dev.shell_last("settings put global adb_wifi_enabled 0; settings put global adb_enabled 0") {
        Ok(()) => true,
        Err(e) => {
            log::warn!("turning ADB off on the TV: {e}");
            false
        }
    };
    installed.map(|()| off)
}

/// Pairs first when a code was typed, then tries port 5555 (network debugging) and the
/// Wireless debugging port. With the error, a problem other than the error's own one.
fn connect(addr: Ipv4Addr, key: &adb::Key, host: &str, wireless: &Wireless, cancel: &AtomicBool, mut on_prompt: impl FnMut()) -> Result<Device, (Option<Problem>, Fail)> {
    let ip = IpAddr::V4(addr);
    let mut paired = false;
    if let Some(code) = &wireless.code {
        let pair_at = wireless.pair_at.or_else(|| adb_wifi::discover(adb_wifi::PAIRING_SERVICE, addr, PAIRING_LOOKUP, cancel).map(|p| SocketAddr::new(ip, p)));
        let Some(pair_at) = pair_at else {
            return Err((Some(Problem::NoPairPort), Fail::Other(format!("no {} announced by {addr}", adb_wifi::PAIRING_SERVICE))));
        };
        let id = adb_wifi::pair(pair_at, key, host, code).map_err(|e| (None, e))?;
        log::info!("paired with {id} at {pair_at}");
        paired = true;
    }
    let mut try_at = |at: SocketAddr| Device::connect(at, key, host, ALLOW_FOR, cancel, &mut on_prompt).map_err(|e| (None, e));
    if let Some(at) = wireless.connect_at {
        return try_at(at);
    }
    let legacy = try_at(SocketAddr::new(ip, adb::PORT));
    if !matches!(legacy, Err((_, Fail::NoAdb(_) | Fail::NeedsPairing))) {
        return legacy;
    }
    match adb_wifi::discover(adb_wifi::CONNECT_SERVICE, addr, CONNECT_LOOKUP, cancel) {
        Some(port) => try_at(SocketAddr::new(ip, port)),
        None if paired => Err((Some(Problem::NoConnectPort), Fail::Other(format!("paired, but no {} announced by {addr}", adb_wifi::CONNECT_SERVICE)))),
        None => legacy,
    }
}

/// Copies the APK and runs `pm install`. The outer error is ADB failing, the inner one a
/// refusal from the package manager.
fn install(dev: &mut Device, apk: &[u8], cancel: &AtomicBool, set: &dyn Fn(Step, Option<f32>)) -> Result<Result<(), Failure>, (Step, Fail)> {
    set(Step::Install, Some(0.0));
    let total = apk.len().max(1) as f32;
    dev.push(apk, REMOTE_APK, cancel, |n| set(Step::Install, Some(n as f32 / total))).map_err(|e| (Step::Install, e))?;
    set(Step::Install, None);
    let mut out = dev.shell(&format!("pm install -r {REMOTE_APK}"), INSTALL_FOR).map_err(|e| (Step::Install, e))?;
    if out.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
        // Another build of the same app (Play Store / older debug key): replace it.
        log::info!("replacing the NANOCast app on the TV signed with another key");
        let _ = dev.shell(&format!("pm uninstall {PACKAGE}"), Duration::from_secs(60));
        out = dev.shell(&format!("pm install -r {REMOTE_APK}"), INSTALL_FOR).map_err(|e| (Step::Install, e))?;
    }
    let _ = dev.shell(&format!("rm -f {REMOTE_APK}"), Duration::from_secs(10));
    Ok(match pm_result(&out) {
        Ok(()) => Ok(()),
        Err(problem) => Err(Failure { problem, step: Step::Install, detail: format!("pm install: {}", out.trim()) }),
    })
}

/// `pm install` output to success or a problem. A newer app already on the TV is fine.
fn pm_result(out: &str) -> Result<(), Problem> {
    if out.contains("Success") || out.contains("INSTALL_FAILED_VERSION_DOWNGRADE") {
        Ok(())
    } else if out.contains("INSUFFICIENT_STORAGE") {
        Err(Problem::LowSpace)
    } else {
        Err(Problem::Failed)
    }
}

/// `%APPDATA%\NANOCast` (key) — next to config.json.
fn data_dir() -> PathBuf {
    directories::BaseDirs::new().map(|d| d.config_dir().join("NANOCast")).unwrap_or_else(|| PathBuf::from("."))
}

/// The APK: `NANOCAST_APK` (for testing), a copy next to the program, the download cache,
/// or downloaded now (and cached for the next TV in the room).
fn get_apk(url: &str, cancel: &AtomicBool, progress: impl Fn(f32)) -> Result<Vec<u8>, String> {
    let file = url.rsplit('/').next().unwrap_or("receiver.apk").to_string();
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(&file)));
    let cache = directories::BaseDirs::new().map(|d| d.cache_dir().join("NANOCast").join(&file));
    let local = std::env::var_os("NANOCAST_APK").map(PathBuf::from).into_iter().chain(beside).chain(cache.clone());
    for path in local {
        if let Ok(bytes) = std::fs::read(&path) {
            if is_apk(&bytes) {
                log::info!("receiver APK from {}", path.display());
                return Ok(bytes);
            }
        }
    }
    log::info!("downloading {url}");
    let bytes = download(url, MAX_APK, cancel, progress)?;
    if !is_apk(&bytes) {
        return Err(format!("{url}: not an APK ({} bytes)", bytes.len()));
    }
    if let Some(path) = cache {
        let tmp = path.with_extension("part");
        let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&tmp, &bytes)).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = saved {
            log::warn!("caching the APK in {}: {e}", path.display());
        }
    }
    Ok(bytes)
}

fn is_apk(bytes: &[u8]) -> bool {
    bytes.len() > 1024 && bytes.starts_with(b"PK\x03\x04")
}

/// GET `url` (at most `max` bytes), with progress 0..=1 when the size is known. Also used
/// for app updates (`update.rs`).
pub fn download(url: &str, max: u64, cancel: &AtomicBool, progress: impl Fn(f32)) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_connect(Some(Duration::from_secs(10))).timeout_recv_body(Some(Duration::from_secs(60))).build().into();
    let resp = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok()).filter(|&n| n > 0);
    let mut reader = resp.into_body().into_reader().take(max);
    let mut out = Vec::with_capacity(total.unwrap_or(4 << 20) as usize);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if let Some(t) = total {
            progress((out.len() as f32 / t as f32).min(1.0));
        }
    }
    if total.is_some_and(|t| t != out.len() as u64) {
        return Err(format!("{url}: got {} of {} bytes", out.len(), total.unwrap_or_default()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_manager_answers() {
        assert_eq!(pm_result("Performing Streamed Install\nSuccess\n"), Ok(()));
        assert_eq!(pm_result("Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"), Ok(()));
        assert_eq!(pm_result("Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"), Err(Problem::LowSpace));
        assert_eq!(pm_result("Failure [INSTALL_FAILED_INVALID_APK]"), Err(Problem::Failed));
    }

    #[test]
    fn steps_in_order() {
        assert!(Step::ALL.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn typed_tv_ip() {
        let ip = Some(Ipv4Addr::new(192, 168, 1, 50));
        for typed in ["192.168.1.50", " 192.168.1.50 ", "192,168,1,50", "192.168.1.50:5555", "http://192.168.1.50/", "192.168.001.050", "192. 168. 1. 50"] {
            assert_eq!(parse_tv_ip(typed), ip, "{typed}");
        }
        for typed in ["", "192.168.1", "192.168.1.256", "192.168.1.50.7", "tv.local", "0.0.0.0", "255.255.255.255", "1234.1.1.1"] {
            assert_eq!(parse_tv_ip(typed), None, "{typed}");
        }
    }

    #[test]
    fn typed_addresses() {
        let tv = Ipv4Addr::new(192, 168, 1, 20);
        assert_eq!(parse_address(" 192.168.1.30:37123 ", tv), Some("192.168.1.30:37123".parse().unwrap()));
        assert_eq!(parse_address("41234", tv), Some("192.168.1.20:41234".parse().unwrap()));
        assert_eq!(parse_address("", tv), None);
        assert_eq!(parse_address("0", tv), None);
        assert_eq!(parse_address("192.168.1.30", tv), None);
    }

    /// Code typed: pair on the dialog's port, then ADB over TLS on the debugging port.
    #[test]
    fn pair_with_code_then_connect() {
        use crate::adb::tests::{fake_tls_tv, other_key, test_key};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pair_at = listener.local_addr().unwrap();
        let pairing = std::thread::spawn(move || adb_wifi::fake::serve_pairing(listener.accept().unwrap().0, other_key(), "246802", "adb-TV"));
        let (connect_at, tv) = fake_tls_tv(vec![adb_wifi::fake::spki(test_key())], |tls| assert!(tls.is_some()));
        let wireless = Wireless { code: Some("246802".into()), pair_at: Some(pair_at), connect_at: Some(connect_at) };
        let dev = connect(Ipv4Addr::LOCALHOST, test_key(), "test@pc", &wireless, &AtomicBool::new(false), || panic!("no Allow dialog with TLS"));
        assert_eq!(dev.map_err(|(_, e)| e.to_string()).unwrap().model().as_deref(), Some("TLS TV"));
        assert!(pairing.join().unwrap().is_some());
        tv.join().unwrap();
    }

    #[test]
    fn wrong_code_is_reported() {
        use crate::adb::tests::{other_key, test_key};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pair_at = listener.local_addr().unwrap();
        let _pairing = std::thread::spawn(move || adb_wifi::fake::serve_pairing(listener.accept().unwrap().0, other_key(), "246802", "adb-TV"));
        let wireless = Wireless { code: Some("999999".into()), pair_at: Some(pair_at), connect_at: None };
        let r = connect(Ipv4Addr::LOCALHOST, test_key(), "test@pc", &wireless, &AtomicBool::new(false), || {});
        assert!(matches!(r, Err((None, Fail::WrongCode))));
    }
}
