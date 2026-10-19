//! X11 capture back end and X11 source listing (x11rb, pure Rust, no libX11 needed).
//!
//! - Monitor: `ShmGetImage` of the root window over the RandR monitor rectangle.
//! - Window: the Composite extension keeps an off-screen pixmap of the window
//!   (`NameWindowPixmap`), so the picture is right even when other windows cover it; without
//!   Composite the window's on-screen rectangle of the root window is captured instead
//!   (covering windows then show up). A minimized window keeps its last frame.
//! - The cursor is not part of `GetImage`; it is drawn in from `XFixesGetCursorImage`.
//! - Without MIT-SHM (remote display) plain `GetImage` is used: slower, still works.
//!
//! X11 has no change notification here, so frames are grabbed on a fixed clock at the
//! target fps (the encoder makes static frames tiny). Only 24/32-bit TrueColor with
//! 32 bits per pixel, little-endian (BGRx in memory) is supported: every desktop GPU today.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use log::{debug, info, warn};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::capture::{CaptureFlags, CapturedFrame, Pacer, Source};
use crate::convert::{blend_cursor, rgb32_to_nv12_fit, Rgb32};

/// One RandR monitor (or the whole screen when RandR 1.5 is missing).
#[derive(Clone, Debug)]
pub(crate) struct Monitor {
    pub name: String,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

/// One top-level application window from `_NET_CLIENT_LIST`.
#[derive(Clone, Debug)]
pub(crate) struct ClientWindow {
    pub id: Window,
    pub title: String,
    pub process: String,
    pub pid: Option<u32>,
    pub minimized: bool,
    pub width: u16,
    pub height: u16,
}

pub(crate) fn connect() -> Result<(RustConnection, Window)> {
    let (conn, screen) = x11rb::connect(None).context("cannot open the X11 display (is DISPLAY set?)")?;
    let root = conn.setup().roots.get(screen).ok_or_else(|| anyhow!("X11 screen {screen} missing"))?.root;
    Ok((conn, root))
}

fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn atom_name(conn: &RustConnection, atom: u32) -> Option<String> {
    let reply = conn.get_atom_name(atom).ok()?.reply().ok()?;
    Some(String::from_utf8_lossy(&reply.name).into_owned())
}

/// RandR monitors in server order (the index used by `Source::Monitor`).
pub(crate) fn monitors(conn: &RustConnection, root: Window) -> Vec<Monitor> {
    let randr = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.monitors)
        .unwrap_or_default();
    if !randr.is_empty() {
        return randr
            .iter()
            .enumerate()
            .map(|(i, m)| Monitor {
                name: atom_name(conn, m.name).unwrap_or_else(|| format!("Màn hình {}", i + 1)),
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                primary: m.primary,
            })
            .collect();
    }
    // No RandR 1.5 (very old server or Xvfb): the whole screen is one monitor.
    let screen = conn.setup().roots.iter().find(|s| s.root == root);
    screen
        .map(|s| Monitor {
            name: "Màn hình 1".into(),
            x: 0,
            y: 0,
            width: s.width_in_pixels,
            height: s.height_in_pixels,
            primary: true,
        })
        .into_iter()
        .collect()
}

fn property32(conn: &RustConnection, win: Window, prop: u32, kind: impl Into<u32>) -> Vec<u32> {
    conn.get_property(false, win, prop, kind.into(), 0, u32::MAX / 4)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

fn window_title(conn: &RustConnection, win: Window, net_wm_name: u32, utf8: u32) -> String {
    for (prop, kind) in [(net_wm_name, utf8), (AtomEnum::WM_NAME.into(), AtomEnum::ANY.into())] {
        if let Some(r) = conn.get_property(false, win, prop, kind, 0, 4096).ok().and_then(|c| c.reply().ok()) {
            if !r.value.is_empty() {
                return String::from_utf8_lossy(&r.value).trim().to_string();
            }
        }
    }
    String::new()
}

fn process_name(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Application windows managed by the window manager (EWMH `_NET_CLIENT_LIST`).
pub(crate) fn client_windows(conn: &RustConnection, root: Window) -> Result<Vec<ClientWindow>> {
    let client_list = atom(conn, "_NET_CLIENT_LIST")?;
    let net_wm_name = atom(conn, "_NET_WM_NAME")?;
    let utf8 = atom(conn, "UTF8_STRING")?;
    let net_wm_pid = atom(conn, "_NET_WM_PID")?;
    let net_wm_state = atom(conn, "_NET_WM_STATE")?;
    let hidden = atom(conn, "_NET_WM_STATE_HIDDEN")?;
    let skip_taskbar = atom(conn, "_NET_WM_STATE_SKIP_TASKBAR")?;
    let wm_type = atom(conn, "_NET_WM_WINDOW_TYPE")?;
    let type_normal = atom(conn, "_NET_WM_WINDOW_TYPE_NORMAL")?;
    let type_dialog = atom(conn, "_NET_WM_WINDOW_TYPE_DIALOG")?;

    let ids = property32(conn, root, client_list, AtomEnum::WINDOW);
    if ids.is_empty() {
        bail!("the window manager does not publish _NET_CLIENT_LIST");
    }
    let mut out = Vec::new();
    for id in ids {
        let types = property32(conn, id, wm_type, AtomEnum::ATOM);
        if !types.is_empty() && !types.iter().any(|t| *t == type_normal || *t == type_dialog) {
            continue; // docks, desktop, splash screens...
        }
        let state = property32(conn, id, net_wm_state, AtomEnum::ATOM);
        if state.contains(&skip_taskbar) {
            continue;
        }
        let Ok(geo) = conn.get_geometry(id).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) else {
            continue;
        };
        let pid = property32(conn, id, net_wm_pid, AtomEnum::CARDINAL).first().copied();
        out.push(ClientWindow {
            id,
            title: window_title(conn, id, net_wm_name, utf8),
            process: pid.map(process_name).unwrap_or_default(),
            pid,
            minimized: state.contains(&hidden),
            width: geo.width,
            height: geo.height,
        });
    }
    Ok(out)
}

pub(crate) fn window_exists(conn: &RustConnection, win: Window) -> bool {
    conn.get_window_attributes(win).ok().and_then(|c| c.reply().ok()).is_some()
}

/// What `Target` grabs from.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Monitor { x: i16, y: i16, width: u16, height: u16 },
    Window(Window),
}

/// An opened X11 capture target (see `capture::Prepared`).
pub(crate) struct Target {
    conn: RustConnection,
    root: Window,
    kind: Kind,
    size: (u32, u32),
}

impl Target {
    pub(crate) fn open(source: &Source) -> Result<Self> {
        let (conn, root) = connect()?;
        let (kind, size) = match *source {
            Source::Monitor(index) => {
                let mons = monitors(&conn, root);
                let m = mons.get(index.wrapping_sub(1)).ok_or_else(|| anyhow!("monitor {index} not found"))?;
                (Kind::Monitor { x: m.x, y: m.y, width: m.width, height: m.height }, (m.width as u32, m.height as u32))
            }
            Source::Window(id) => {
                if id <= 0 || id > u32::MAX as isize {
                    bail!("window id {id} is not an X11 window (the system dialog choice needs the portal back end)");
                }
                let win = id as Window;
                let geo = conn.get_geometry(win)?.reply().map_err(|_| anyhow!("the window is gone"))?;
                (Kind::Window(win), (geo.width as u32, geo.height as u32))
            }
        };
        check_pixel_format(&conn)?;
        Ok(Self { conn, root, kind, size })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.size
    }

    pub(crate) fn start(self, flags: CaptureFlags) -> Result<Handle> {
        let stop = Arc::new(AtomicBool::new(false));
        let counters = flags.counters.clone();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new().name("x11-capture".into()).spawn(move || {
                let counters = flags.counters.clone();
                if let Err(e) = run(self, &flags, &stop) {
                    warn!("X11 capture ended: {e:#}");
                }
                counters.closed.store(true, Ordering::Relaxed);
            })?
        };
        Ok(Handle { stop, thread: Some(thread), counters })
    }
}

pub(crate) struct Handle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<crate::capture::CaptureCounters>,
}

impl Handle {
    pub(crate) fn is_finished(&self) -> bool {
        // `closed` is also set by a clean stop, but then nobody asks any more.
        self.counters.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        self.stop_inner();
        Ok(())
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// The capture loop assumes 32 bits per pixel, little-endian (B, G, R, x in memory).
fn check_pixel_format(conn: &RustConnection) -> Result<()> {
    let setup = conn.setup();
    if setup.image_byte_order != ImageOrder::LSB_FIRST {
        bail!("big-endian X11 image order is not supported");
    }
    let bpp24 = setup.pixmap_formats.iter().find(|f| f.depth == 24).map(|f| f.bits_per_pixel);
    if bpp24 != Some(32) {
        bail!("X11 depth 24 uses {bpp24:?} bits per pixel; only 32 is supported");
    }
    Ok(())
}

/// A System V shared memory segment attached to the X server.
struct ShmBuf {
    seg: shm::Seg,
    shmid: i32,
    addr: *mut u8,
    size: usize,
}

impl ShmBuf {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                bail!("shmget({size}): {}", std::io::Error::last_os_error());
            }
            let addr = libc::shmat(shmid, std::ptr::null(), 0);
            if addr as isize == -1 {
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                bail!("shmat: {}", std::io::Error::last_os_error());
            }
            let seg = match conn.generate_id() {
                Ok(id) => id,
                Err(e) => {
                    libc::shmdt(addr);
                    libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                    return Err(e.into());
                }
            };
            let attached = conn.shm_attach(seg, shmid as u32, false).map_err(anyhow::Error::from).and_then(|c| Ok(c.check()?));
            // Marked for removal now: it disappears once both sides detach (also on a crash).
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            if let Err(e) = attached {
                libc::shmdt(addr);
                return Err(e).context("ShmAttach");
            }
            Ok(Self { seg, shmid, addr: addr as *mut u8, size })
        }
    }

    fn free(self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg);
        let _ = conn.flush();
        unsafe {
            libc::shmdt(self.addr as *const libc::c_void);
        }
        debug!("freed shm segment {} ({} bytes)", self.shmid, self.size);
    }

    /// The segment contents. The X server writes only during `ShmGetImage`, which has
    /// completed (reply received) before this is read.
    fn bytes(&mut self, len: usize) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.addr, len.min(self.size)) }
    }
}

/// Grabs `width`x`height` pixels at (`x`, `y`) of `drawable` (BGRx, stride `width*4`).
struct Grabber {
    shm: Option<ShmBuf>,
    shm_available: bool,
    fallback: Vec<u8>,
}

impl Grabber {
    fn new(conn: &RustConnection) -> Self {
        let shm_available = conn.extension_information(shm::X11_EXTENSION_NAME).ok().flatten().is_some()
            && conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
        if !shm_available {
            warn!("X11 MIT-SHM unavailable (remote display?); using plain GetImage, expect high CPU");
        }
        Self { shm: None, shm_available, fallback: Vec::new() }
    }

    fn grab(&mut self, conn: &RustConnection, drawable: u32, x: i16, y: i16, width: u16, height: u16) -> Result<&mut [u8]> {
        let len = width as usize * height as usize * 4;
        if self.shm_available {
            if self.shm.as_ref().is_none_or(|s| s.size < len) {
                if let Some(old) = self.shm.take() {
                    old.free(conn);
                }
                match ShmBuf::new(conn, len) {
                    Ok(s) => self.shm = Some(s),
                    Err(e) => {
                        warn!("MIT-SHM setup failed ({e:#}); using plain GetImage");
                        self.shm_available = false;
                    }
                }
            }
            if let Some(shm) = self.shm.as_mut() {
                conn.shm_get_image(drawable, x, y, width, height, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)?.reply()?;
                return Ok(shm.bytes(len));
            }
        }
        let reply = conn.get_image(ImageFormat::Z_PIXMAP, drawable, x, y, width, height, !0)?.reply()?;
        self.fallback = reply.data;
        if self.fallback.len() < len {
            bail!("GetImage returned {} bytes for {width}x{height}", self.fallback.len());
        }
        Ok(&mut self.fallback[..len])
    }

    fn free(mut self, conn: &RustConnection) {
        if let Some(s) = self.shm.take() {
            s.free(conn);
        }
    }
}

/// Window content through Composite: a pixmap named for the current window size.
struct CompositeWindow {
    win: Window,
    pixmap: Option<(xproto::Pixmap, u16, u16)>,
}

impl CompositeWindow {
    fn new(conn: &RustConnection, win: Window) -> Option<Self> {
        conn.extension_information(composite::X11_EXTENSION_NAME).ok().flatten()?;
        let v = conn.composite_query_version(0, 4).ok()?.reply().ok()?;
        if v.major_version == 0 && v.minor_version < 2 {
            return None;
        }
        // Automatic redirection keeps the window on screen as usual; with a compositing
        // window manager the window is already redirected and this changes nothing.
        conn.composite_redirect_window(win, composite::Redirect::AUTOMATIC).ok()?.check().ok()?;
        Some(Self { win, pixmap: None })
    }

    /// The pixmap holding the window picture at `width`x`height` (renamed after a resize).
    fn pixmap(&mut self, conn: &RustConnection, width: u16, height: u16) -> Result<xproto::Pixmap> {
        if let Some((p, w, h)) = self.pixmap {
            if w == width && h == height {
                return Ok(p);
            }
            let _ = conn.free_pixmap(p);
            self.pixmap = None;
        }
        let p = conn.generate_id()?;
        conn.composite_name_window_pixmap(self.win, p)?.check()?;
        self.pixmap = Some((p, width, height));
        Ok(p)
    }

    /// The pixmap is invalid once the window is unmapped (minimized).
    fn invalidate(&mut self, conn: &RustConnection) {
        if let Some((p, _, _)) = self.pixmap.take() {
            let _ = conn.free_pixmap(p);
        }
    }

    fn free(mut self, conn: &RustConnection) {
        self.invalidate(conn);
        let _ = conn.composite_unredirect_window(self.win, composite::Redirect::AUTOMATIC);
        let _ = conn.flush();
    }
}

fn run(target: Target, flags: &CaptureFlags, stop: &AtomicBool) -> Result<()> {
    let Target { conn, root, kind, .. } = target;
    let mut grabber = Grabber::new(&conn);
    let cursor = conn.extension_information(xfixes::X11_EXTENSION_NAME).ok().flatten().is_some()
        && conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
    let mut composite = match kind {
        Kind::Window(win) => CompositeWindow::new(&conn, win),
        Kind::Monitor { .. } => None,
    };
    info!(
        "X11 capture {kind:?}: MIT-SHM {}, cursor {}, composite {}",
        grabber.shm_available,
        cursor,
        composite.is_some()
    );

    let (cw, ch) = flags.canvas;
    let mut pacer = Pacer::new();
    let mut last_error_log = Instant::now() - Duration::from_secs(10);
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if flags.is_paused() {
            pacer.reset();
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        pacer.wait(Duration::from_millis(50));
        let now = Instant::now();
        if !pacer.accept(now, flags.interval()) {
            continue;
        }

        // Where to read from this time.
        let region = match kind {
            Kind::Monitor { x, y, width, height } => Some((root, x, y, width, height, x, y)),
            Kind::Window(win) => match window_region(&conn, root, win, composite.as_mut()) {
                Ok(r) => r,
                Err(e) => break Err(e.context("the window is gone")),
            },
        };
        let Some((drawable, x, y, width, height, origin_x, origin_y)) = region else {
            continue; // minimized / unmapped: keep the last frame on the TV
        };
        if width < 16 || height < 16 {
            continue;
        }

        let pixels = match grabber.grab(&conn, drawable, x, y, width, height) {
            Ok(p) => p,
            Err(e) => {
                if let Some(c) = composite.as_mut() {
                    c.invalidate(&conn);
                }
                if last_error_log.elapsed() > Duration::from_secs(5) {
                    warn!("X11 grab failed: {e:#}");
                    last_error_log = Instant::now();
                }
                continue;
            }
        };
        let stride = width as usize * 4;
        if cursor {
            draw_cursor(&conn, pixels, width, height, stride, origin_x, origin_y);
        }

        let t = Instant::now();
        let mut nv12 = vec![0u8; (cw * ch * 3 / 2) as usize];
        rgb32_to_nv12_fit(pixels, width as u32, height as u32, stride, Rgb32::Bgrx, cw, ch, &mut nv12);
        let convert_us = t.elapsed().as_micros() as u64;
        if !flags.deliver(CapturedFrame { nv12, width: cw, height: ch, captured: now, convert_us }) {
            break Ok(());
        }
    };

    if let Some(c) = composite.take() {
        c.free(&conn);
    }
    grabber.free(&conn);
    result
}

/// (drawable, x, y, width, height, screen x of the picture, screen y of the picture) for a
/// window, `None` while it is not viewable, an error once it no longer exists.
#[allow(clippy::type_complexity)]
fn window_region(
    conn: &RustConnection,
    root: Window,
    win: Window,
    composite: Option<&mut CompositeWindow>,
) -> Result<Option<(u32, i16, i16, u16, u16, i16, i16)>> {
    let attrs = conn.get_window_attributes(win)?.reply()?;
    let geo = conn.get_geometry(win)?.reply()?;
    let pos = conn.translate_coordinates(win, root, 0, 0)?.reply()?;
    if attrs.map_state != MapState::VIEWABLE {
        if let Some(c) = composite {
            c.invalidate(conn);
        }
        return Ok(None);
    }
    if let Some(c) = composite {
        let pixmap = c.pixmap(conn, geo.width, geo.height)?;
        return Ok(Some((pixmap, 0, 0, geo.width, geo.height, pos.dst_x, pos.dst_y)));
    }
    // No Composite: the window's rectangle of the screen, clipped to the screen.
    let screen = conn.setup().roots.iter().find(|s| s.root == root).ok_or_else(|| anyhow!("root window not found"))?;
    let (sw, sh) = (screen.width_in_pixels as i32, screen.height_in_pixels as i32);
    let x0 = (pos.dst_x as i32).clamp(0, sw);
    let y0 = (pos.dst_y as i32).clamp(0, sh);
    let x1 = (pos.dst_x as i32 + geo.width as i32).clamp(0, sw);
    let y1 = (pos.dst_y as i32 + geo.height as i32).clamp(0, sh);
    if x1 - x0 < 16 || y1 - y0 < 16 {
        return Ok(None);
    }
    Ok(Some((root, x0 as i16, y0 as i16, (x1 - x0) as u16, (y1 - y0) as u16, x0 as i16, y0 as i16)))
}

/// Draws the current cursor onto the grabbed picture whose top-left is at screen
/// (`origin_x`, `origin_y`).
fn draw_cursor(conn: &RustConnection, pixels: &mut [u8], width: u16, height: u16, stride: usize, origin_x: i16, origin_y: i16) {
    let Some(img) = conn.xfixes_get_cursor_image().ok().and_then(|c| c.reply().ok()) else { return };
    let x = img.x as i32 - img.xhot as i32 - origin_x as i32;
    let y = img.y as i32 - img.yhot as i32 - origin_y as i32;
    blend_cursor(
        pixels,
        width as u32,
        height as u32,
        stride,
        Rgb32::Bgrx,
        &img.cursor_image,
        img.width as u32,
        img.height as u32,
        x,
        y,
    );
}
