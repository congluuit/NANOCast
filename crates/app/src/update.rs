//! Updates from GitHub releases. Every few hours: which tag does `releases/latest` redirect
//! to? When it is newer than this build, download this copy's file in the background; it is
//! installed when the user presses "Update" (NANOCast opens again afterwards) or, with
//! automatic updates on, quietly when NANOCast closes.
//!
//! - Windows, installed (Inno Setup's `unins000.exe` next to the program):
//!   `NANOCast-Setup-<v>.exe /SILENT` installs over this copy, `/relaunch=1` opens it again.
//! - Windows, portable: `NANOCast-<v>-portable-win64.exe` takes the place of the running
//!   exe (Windows lets a running exe be renamed, not overwritten).
//! - macOS, Linux and builds under cargo's `target`: only says so and opens the release page.
//!
//! No API token and no JSON: the redirect of `releases/latest` is not rate-limited like
//! api.github.com (60 requests an hour per IP, a whole school behind one address), and the
//! file names come from `release.yml`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tv_install;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = env!("CARGO_PKG_REPOSITORY");
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// After a failed check (no network, release still uploading).
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);
const MAX_DOWNLOAD: u64 = 128 * 1024 * 1024;

/// How this copy of NANOCast can be replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// Installed by `installer/nanocast.iss`; `all_users`: in Program Files (asks for admin).
    Setup { all_users: bool },
    /// The portable exe, anywhere the user put it.
    Portable,
    /// Download page only.
    Manual,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    /// `version` is out but this copy cannot install it itself.
    Manual { version: String },
    Downloading { version: String, fraction: Option<f32> },
    Ready { version: String, file: PathBuf },
    Failed(String),
}

pub struct Updater {
    state: Arc<Mutex<State>>,
    install: Install,
    checked_at: Option<Instant>,
    cancel: Arc<AtomicBool>,
}

impl Updater {
    pub fn new() -> Self {
        let install = detect();
        log::info!("NANOCast {CURRENT}, updates: {install:?}");
        clean_up(install);
        Self { state: Arc::new(Mutex::new(State::Idle)), install, checked_at: None, cancel: Arc::new(AtomicBool::new(false)) }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn busy(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Checking | State::Downloading { .. })
    }

    /// Every frame: checks when one is due and `auto` is on.
    pub fn tick(&mut self, auto: bool) {
        let wait = if matches!(self.state(), State::Failed(_)) { RETRY_AFTER } else { CHECK_EVERY };
        if auto && self.checked_at.is_none_or(|at| at.elapsed() >= wait) {
            self.check_now();
        }
    }

    /// Asks GitHub now, then downloads the newer version if there is one.
    pub fn check_now(&mut self) {
        if self.busy() || matches!(self.state(), State::Ready { .. }) {
            return;
        }
        self.checked_at = Some(Instant::now());
        *self.state.lock().unwrap() = State::Checking;
        let (state, cancel, install) = (self.state.clone(), self.cancel.clone(), self.install);
        let spawned = std::thread::Builder::new().name("update".into()).spawn(move || {
            let result = check_and_download(install, &state, &cancel);
            match &result {
                Ok(s) => log::info!("update check: {s:?}"),
                Err(e) => log::warn!("update check: {e}"),
            }
            *state.lock().unwrap() = result.unwrap_or_else(State::Failed);
        });
        if let Err(e) = spawned {
            *self.state.lock().unwrap() = State::Failed(format!("update thread: {e}"));
        }
    }

    /// Starts installing the downloaded update; NANOCast must quit right after `Ok`.
    /// `relaunch`: open NANOCast again afterwards (else installs without any window).
    pub fn apply(&self, relaunch: bool) -> Result<(), String> {
        let State::Ready { file, version } = self.state() else {
            return Err("no update downloaded".into());
        };
        log::info!("installing NANOCast {version} from {} (relaunch: {relaunch})", file.display());
        match self.install {
            Install::Setup { all_users } => run_setup(&file, all_users, relaunch),
            Install::Portable => {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                replace_exe(&exe, &file)?;
                if relaunch {
                    std::process::Command::new(&exe).spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
                }
                Ok(())
            }
            Install::Manual => Err("cannot install here".into()),
        }
    }

    /// Installing when NANOCast closes, without asking: not when that needs admin rights.
    pub fn applies_quietly(&self) -> bool {
        matches!(self.install, Install::Setup { all_users: false } | Install::Portable)
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The page of the release with `version` (or the newest one).
pub fn release_page(version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{REPO}/releases/tag/v{v}"),
        None => format!("{REPO}/releases/latest"),
    }
}

fn check_and_download(install: Install, state: &Mutex<State>, cancel: &AtomicBool) -> Result<State, String> {
    let version = latest_version()?;
    if !is_newer(&version, CURRENT) {
        return Ok(State::UpToDate);
    }
    let Some(name) = file_name(install, &version) else {
        return Ok(State::Manual { version });
    };
    let dir = updates_dir().ok_or("no cache folder")?;
    let file = dir.join(&name);
    // Downloaded earlier (written whole, then renamed).
    if std::fs::read(&file).is_ok_and(|b| is_exe(&b)) {
        return Ok(State::Ready { version, file });
    }
    *state.lock().unwrap() = State::Downloading { version: version.clone(), fraction: None };
    let url = format!("{REPO}/releases/download/v{version}/{name}");
    log::info!("downloading {url}");
    let bytes = tv_install::download(&url, MAX_DOWNLOAD, cancel, |f| {
        if let State::Downloading { fraction, .. } = &mut *state.lock().unwrap() {
            *fraction = Some(f);
        }
    })?;
    if !is_exe(&bytes) {
        return Err(format!("{url}: not a program ({} bytes)", bytes.len()));
    }
    let tmp = file.with_extension("part");
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, &file))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(State::Ready { version, file })
}

/// The version `releases/latest` redirects to.
fn latest_version() -> Result<String, String> {
    let url = format!("{REPO}/releases/latest");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("NANOCast/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let resp = agent.get(&url).call().map_err(|e| format!("{url}: {e}"))?;
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| format!("{url}: {} without a redirect", resp.status()))?;
    version_from_location(location).ok_or_else(|| format!("{url}: redirects to {location}"))
}

/// ".../releases/tag/v0.2.0" -> "0.2.0".
fn version_from_location(location: &str) -> Option<String> {
    let tag = location.trim_end_matches('/').rsplit_once("/tag/")?.1;
    let v = tag.strip_prefix('v').unwrap_or(tag);
    parse_version(v).map(|_| v.to_string())
}

/// "1.2.3" (or "v1.2", "1.2.3-beta") -> (1, 2, 3).
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim();
    let core = s.strip_prefix('v').unwrap_or(s).split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut next = || parts.next().map(|p| p.parse::<u32>().ok());
    let v = (next()??, next().unwrap_or(Some(0))?, next().unwrap_or(Some(0))?);
    next().is_none().then_some(v)
}

pub fn is_newer(version: &str, than: &str) -> bool {
    matches!((parse_version(version), parse_version(than)), (Some(a), Some(b)) if a > b)
}

/// The release file this copy installs (names from `release.yml`).
fn file_name(install: Install, version: &str) -> Option<String> {
    match install {
        Install::Setup { .. } => Some(format!("NANOCast-Setup-{version}.exe")),
        Install::Portable => Some(format!("NANOCast-{version}-portable-win64.exe")),
        Install::Manual => None,
    }
}

fn is_exe(bytes: &[u8]) -> bool {
    bytes.len() > 100 * 1024 && bytes.starts_with(b"MZ")
}

/// `%LOCALAPPDATA%\NANOCast\updates`.
fn updates_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.cache_dir().join("NANOCast").join("updates"))
}

fn detect() -> Install {
    if !cfg!(windows) {
        return Install::Manual;
    }
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return Install::Manual;
    };
    // Built from source: `cargo build` replaces it.
    if dir.ancestors().any(|a| a.file_name().is_some_and(|n| n == "target") && a.parent().is_some_and(|p| p.join("Cargo.toml").exists())) {
        return Install::Manual;
    }
    if dir.join("unins000.exe").exists() {
        let lower = |p: &Path| p.to_string_lossy().to_lowercase();
        let all_users = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .any(|pf| lower(&dir).starts_with(&lower(Path::new(&pf))));
        return Install::Setup { all_users };
    }
    Install::Portable
}

/// Removes what earlier updates left: the previous portable exe and installed downloads.
fn clean_up(install: Install) {
    if install == Install::Portable {
        if let Ok(exe) = std::env::current_exe() {
            // Fails while the old process is still closing; the next start gets it.
            let _ = std::fs::remove_file(old_exe(&exe));
        }
    }
    let Some(entries) = updates_dir().and_then(|d| std::fs::read_dir(d).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let newer = name.split(['-', '_']).map(|p| p.trim_end_matches(".exe")).any(|p| p.contains('.') && is_newer(p, CURRENT));
        if !newer {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Puts `new` where the running `exe` is; the running one becomes `nanocast.exe.old`.
fn replace_exe(exe: &Path, new: &Path) -> Result<(), String> {
    let old = old_exe(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
    if let Err(e) = std::fs::copy(new, exe) {
        let _ = std::fs::remove_file(exe);
        let _ = std::fs::rename(&old, exe);
        return Err(format!("{}: {e}", exe.display()));
    }
    let _ = std::fs::remove_file(new);
    Ok(())
}

/// Inno Setup without questions; `/relaunch=1` is read by `installer/nanocast.iss`.
fn run_setup(setup: &Path, all_users: bool, relaunch: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new(setup);
    cmd.args([if relaunch { "/SILENT" } else { "/VERYSILENT" }, "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/CLOSEAPPLICATIONS"]);
    cmd.arg(if all_users { "/ALLUSERS" } else { "/CURRENTUSER" });
    cmd.arg(format!("/relaunch={}", u8::from(relaunch)));
    cmd.spawn().map(drop).map_err(|e| format!("{}: {e}", setup.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.10"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("nonsense", "0.1.0"));
    }

    #[test]
    fn latest_redirect() {
        let loc = "https://github.com/congluuit/NANOCast/releases/tag/v0.2.0";
        assert_eq!(version_from_location(loc).as_deref(), Some("0.2.0"));
        assert_eq!(version_from_location("https://github.com/congluuit/NANOCast/releases"), None);
        assert_eq!(version_from_location("/releases/tag/nightly"), None);
    }

    /// `cargo test -p nanocast -- --ignored latest_from_github` (needs the internet).
    #[test]
    #[ignore]
    fn latest_from_github() {
        let v = latest_version().unwrap();
        assert!(parse_version(&v).is_some(), "{v}");
    }

    #[test]
    fn release_files() {
        assert_eq!(file_name(Install::Setup { all_users: false }, "0.2.0").as_deref(), Some("NANOCast-Setup-0.2.0.exe"));
        assert_eq!(file_name(Install::Portable, "0.2.0").as_deref(), Some("NANOCast-0.2.0-portable-win64.exe"));
        assert_eq!(file_name(Install::Manual, "0.2.0"), None);
        assert_eq!(release_page(Some("0.2.0")), "https://github.com/congluuit/NANOCast/releases/tag/v0.2.0");
    }

    #[test]
    fn replaces_the_exe() {
        let dir = std::env::temp_dir().join(format!("nanocast-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("nanocast.exe"), dir.join("download.exe"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        replace_exe(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old_exe(&exe)).unwrap(), "old");
        assert!(!new.exists());
        // Nothing to copy: the old exe is put back.
        assert!(replace_exe(&exe, &dir.join("missing.exe")).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
