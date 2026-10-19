//! xdg-desktop-portal ScreenCast session (D-Bus, `ashpd`): asks the desktop for one screen or
//! one window and returns the PipeWire remote + node to read it from (`pw_video.rs`).
//!
//! - The portal shows its own dialog; NANOCast cannot preselect the source.
//! - Screens are remembered (portal version 4+, `persist_mode` = until revoked): the restore
//!   token is stored per monitor index in `$XDG_STATE_HOME/nanocast/portal-tokens`, so the
//!   dialog appears once per index and later casts start straight away. Windows are never
//!   remembered (their identity does not survive restarts).
//! - The session lives on its own thread (async D-Bus driven by `pollster`), which closes it
//!   when the `PortalSession` is dropped. The desktop's "stop sharing" indicator ends the
//!   PipeWire stream, which the capture reports as a closed source.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use log::{info, warn};

use crate::capture::Source;

/// What `open` hands to the PipeWire side.
pub(crate) struct PortalStream {
    /// Connection to the PipeWire instance that serves the shared stream.
    pub fd: OwnedFd,
    pub node_id: u32,
    /// Size the portal reported (logical pixels on some desktops; frames carry the truth).
    pub size: Option<(u32, u32)>,
    pub session: PortalSession,
}

/// Keeps the portal session open; dropping it closes the session.
pub(crate) struct PortalSession {
    close: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        self.close.take();
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

struct Opened {
    fd: OwnedFd,
    node_id: u32,
    size: Option<(u32, u32)>,
}

/// Opens a ScreenCast session for `source`. Blocks while the portal dialog is shown;
/// returns early (with an error) when `stop` is set.
pub(crate) fn open(source: &Source, stop: &AtomicBool) -> Result<PortalStream> {
    let source = source.clone();
    let (result_tx, result_rx) = sync_channel::<Result<Opened, String>>(1);
    let (close_tx, close_rx) = channel::<()>();
    let thread = std::thread::Builder::new().name("portal".into()).spawn(move || {
        session_thread(&source, result_tx, close_rx);
    })?;
    let session = PortalSession { close: Some(close_tx), thread: Some(thread) };
    loop {
        if stop.load(Ordering::Relaxed) {
            // Dropping `session` closes the portal session once the dialog is answered.
            bail!("cancelled while the screen-sharing dialog was open");
        }
        match result_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(o)) => return Ok(PortalStream { fd: o.fd, node_id: o.node_id, size: o.size, session }),
            Ok(Err(e)) => return Err(anyhow!(e)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the portal thread ended unexpectedly"),
        }
    }
}

fn session_thread(source: &Source, result: std::sync::mpsc::SyncSender<Result<Opened, String>>, close: Receiver<()>) {
    pollster::block_on(async {
        let proxy = match Screencast::new().await {
            Ok(p) => p,
            Err(e) => {
                let _ = result.send(Err(format!(
                    "xdg-desktop-portal ScreenCast is not available ({e}); install xdg-desktop-portal and the backend for your desktop (gnome, kde, wlr), or use an X11 session"
                )));
                return;
            }
        };
        let session = match proxy.create_session().await {
            Ok(s) => s,
            Err(e) => {
                let _ = result.send(Err(format!("portal CreateSession: {e}")));
                return;
            }
        };
        match start(&proxy, &session, source).await {
            Ok(opened) => {
                if result.send(Ok(opened)).is_err() {
                    let _ = session.close().await;
                    return;
                }
            }
            Err(e) => {
                let _ = result.send(Err(format!("{e:#}")));
                let _ = session.close().await;
                return;
            }
        }
        // Keep the session until the capture is done (sender dropped).
        let _ = close.recv();
        if let Err(e) = session.close().await {
            warn!("portal session close: {e}");
        }
        info!("portal session closed");
    });
}

async fn start(proxy: &Screencast<'static>, session: &Session<'static, Screencast<'static>>, source: &Source) -> Result<Opened> {
    let version: u32 = proxy.get_property("version").await.unwrap_or(1);
    let available = proxy.available_source_types().await.unwrap_or(SourceType::Monitor.into());
    let (kind, key) = match *source {
        Source::Monitor(i) => (SourceType::Monitor, Some(format!("monitor-{i}"))),
        Source::Window(_) => (SourceType::Window, None),
    };
    if !available.contains(kind) {
        bail!("this desktop's portal cannot share a {kind:?} (offers {available:?}); cast the whole screen instead");
    }
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
        Ok(_) => CursorMode::Hidden,
        // Version 1 portals have no cursor modes: the default (usually embedded) applies.
        Err(_) => CursorMode::Embedded,
    };
    let persist = version >= 4 && key.is_some();
    let token = if persist { key.as_deref().and_then(load_token) } else { None };
    info!(
        "portal ScreenCast v{version}: {kind:?}, cursor {cursor:?}, {}",
        if token.is_some() { "restoring the previous choice" } else { "showing the dialog" }
    );

    let persist_mode = if persist { PersistMode::ExplicitlyRevoked } else { PersistMode::DoNot };
    proxy
        .select_sources(session, cursor, kind.into(), false, token.as_deref(), persist_mode)
        .await
        .and_then(|r| r.response())
        .map_err(describe)
        .context("portal SelectSources")?;
    let streams = proxy.start(session, None).await.and_then(|r| r.response()).map_err(describe).context("portal Start")?;
    if let (true, Some(key), Some(new)) = (persist, key.as_deref(), streams.restore_token()) {
        save_token(key, new);
    }
    let stream = streams.streams().first().ok_or_else(|| anyhow!("the portal returned no stream"))?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size().filter(|&(w, h)| w > 0 && h > 0).map(|(w, h)| (w as u32, h as u32));
    let fd = proxy.open_pipe_wire_remote(session).await.map_err(describe).context("portal OpenPipeWireRemote")?;
    info!("portal stream: PipeWire node {node_id}, size {size:?}, source {:?}", stream.source_type());
    Ok(Opened { fd, node_id, size })
}

fn describe(e: ashpd::Error) -> anyhow::Error {
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => anyhow!("screen sharing was cancelled in the system dialog"),
        other => anyhow!("{other}"),
    }
}

fn tokens_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("nanocast").join("portal-tokens"))
}

fn load_token(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(tokens_path()?).ok()?;
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ').map(str::to_string))
}

/// Restore tokens are single-use: every successful start returns the next one.
fn save_token(key: &str, token: &str) {
    let Some(path) = tokens_path() else { return };
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split(' ').next() != Some(key))
        .map(str::to_string)
        .collect();
    lines.push(format!("{key} {token}"));
    let write = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(&path, lines.join("\n") + "\n"));
    if let Err(e) = write {
        warn!("cannot save the portal restore token to {}: {e}", path.display());
    }
}
