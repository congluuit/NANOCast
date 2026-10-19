//! Portal capture back end: the PipeWire node handed out by xdg-desktop-portal
//! (`portal.rs`), read on a `pw-video` thread running its own PipeWire main loop.
//!
//! Format negotiation offers BGRx, BGRA, RGBx, RGBA and NV12 (compositors send BGRx/BGRA)
//! without DRM modifiers, so the compositor falls back to shared memory (memfd). If it
//! still sends a DMA-BUF it is mapped on the CPU (`DMA_BUF_IOCTL_SYNC` around the read),
//! which is only correct for linear buffers; zero-copy DMA-BUF -> VA-API import is future
//! work (see docs/linux.md). Each accepted frame (newest buffer only, paced to the target
//! fps) is converted to NV12 at the canvas size right in the process callback.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use log::{debug, info, warn};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::buffer::{ChunkFlags, DataType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;
use pw::stream::{Stream, StreamFlags, StreamState};

use crate::capture::{CaptureCounters, CaptureFlags, CapturedFrame, Pacer, Source};
use crate::convert::{nv12_fit, rgb32_to_nv12_fit, Rgb32};
use crate::portal::{self, PortalSession};

/// An answered portal dialog, not streaming yet.
pub(crate) struct Prepared {
    stream: portal::PortalStream,
}

pub(crate) fn prepare(source: &Source, stop: &AtomicBool) -> Result<Prepared> {
    Ok(Prepared { stream: portal::open(source, stop)? })
}

impl Prepared {
    pub(crate) fn size(&self) -> (u32, u32) {
        // Some portals leave the size out; the canvas then defaults to 1080p and frames
        // of any size are scaled into it.
        self.stream.size.unwrap_or((1920, 1080))
    }

    pub(crate) fn start(self, flags: CaptureFlags) -> Result<Handle> {
        let portal::PortalStream { fd, node_id, session, .. } = self.stream;
        let counters = flags.counters.clone();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let thread = std::thread::Builder::new().name("pw-video".into()).spawn(move || {
            let counters = flags.counters.clone();
            if let Err(e) = run(fd, node_id, flags, quit_rx, &ready_tx) {
                warn!("PipeWire video: {e:#}");
                let _ = ready_tx.try_send(Err(format!("{e:#}")));
            }
            counters.closed.store(true, Ordering::Relaxed);
        })?;
        let mut handle = Handle { quit: Some(quit_tx), thread: Some(thread), counters, _session: session };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(e)) => {
                handle.stop_inner();
                Err(anyhow!(e)).context("PipeWire screen-cast stream")
            }
            Err(_) => {
                handle.stop_inner();
                bail!("the PipeWire stream did not connect within 5 s")
            }
        }
    }
}

pub(crate) struct Handle {
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<CaptureCounters>,
    /// Dropped after the stream: closes the portal session.
    _session: PortalSession,
}

impl Handle {
    pub(crate) fn is_finished(&self) -> bool {
        self.counters.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        self.stop_inner();
        Ok(())
    }

    fn stop_inner(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                warn!("PipeWire video thread did not stop within 2 s; leaving it behind");
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Rgb(Rgb32),
    Nv12,
}

struct VideoState {
    flags: CaptureFlags,
    pacer: Pacer,
    /// Negotiated format: layout and size.
    format: Option<(Layout, u32, u32)>,
    streaming_seen: bool,
    warned_dmabuf: bool,
    warned_short: bool,
}

fn run(
    fd: OwnedFd,
    node_id: u32,
    flags: CaptureFlags,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw_main_loop_new")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw_context_new")?;
    let core = context.connect_fd_rc(fd, None).context("connect to the portal's PipeWire remote")?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "nanocast-screen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("pw_stream_new")?;

    let fps = (1_000_000 / flags.interval().as_micros().max(1)) as u32;
    let state = VideoState {
        pacer: Pacer::new(),
        flags,
        format: None,
        streaming_seen: false,
        warned_dmabuf: false,
        warned_short: false,
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, st, old, new| {
            info!("PipeWire screen stream: {old:?} -> {new:?}");
            match new {
                StreamState::Streaming => st.streaming_seen = true,
                // The shared window closed, the user pressed "stop sharing", or the
                // compositor went away.
                StreamState::Error(_) => st.flags.counters.closed.store(true, Ordering::Relaxed),
                StreamState::Unconnected if st.streaming_seen => st.flags.counters.closed.store(true, Ordering::Relaxed),
                _ => {}
            }
        })
        .param_changed(|_, st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let layout = match info.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => Some(Layout::Rgb(Rgb32::Bgrx)),
                VideoFormat::RGBx | VideoFormat::RGBA => Some(Layout::Rgb(Rgb32::Rgbx)),
                VideoFormat::NV12 => Some(Layout::Nv12),
                _ => None,
            };
            let size = info.size();
            info!(
                "PipeWire screen format: {:?} {}x{}, framerate {}/{}",
                info.format(),
                size.width,
                size.height,
                info.framerate().num,
                info.framerate().denom
            );
            st.format = layout.map(|l| (l, size.width, size.height));
            if st.format.is_none() {
                warn!("unsupported PipeWire video format {:?}", info.format());
            }
        })
        .process(on_process)
        .register()
        .context("register stream listener")?;

    let pod = format_pod(fps)?;
    let mut params = [Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad format pod"))?];
    stream
        .connect(spa::utils::Direction::Input, Some(node_id), StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS, &mut params)
        .context("pw_stream_connect")?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    let _ = ready.try_send(Ok(()));
    info!("PipeWire screen stream connecting to node {node_id} (target {fps} fps)");
    mainloop.run();
    let _ = stream.disconnect();
    info!("PipeWire screen stream stopped");
    Ok(())
}

/// EnumFormat: raw video in the formats `convert` handles, any size, up to 1000 fps.
fn format_pod(fps: u32) -> Result<Vec<u8>> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::NV12,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps.max(1), denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    let bytes = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .map_err(|e| anyhow!("serialize format pod: {e:?}"))?
        .0
        .into_inner();
    Ok(bytes)
}

/// A DMA-BUF mapped for reading for the duration of one frame.
struct DmaBufMap {
    fd: i32,
    ptr: *mut libc::c_void,
    len: usize,
}

/// `_IOW('b', 0, struct dma_buf_sync)` and its flags (linux/dma-buf.h).
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

impl DmaBufMap {
    fn new(fd: i32, len: usize) -> Option<Self> {
        if fd < 0 || len == 0 {
            return None;
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let map = Self { fd, ptr, len };
        map.sync(DMA_BUF_SYNC_READ);
        Some(map)
    }

    fn sync(&self, flags: u64) {
        unsafe {
            libc::ioctl(self.fd, DMA_BUF_IOCTL_SYNC as _, &flags as *const u64);
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for DmaBufMap {
    fn drop(&mut self) {
        self.sync(DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END);
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// One plane of the current buffer: its readable bytes (from the chunk offset on) and stride.
struct Plane<'a> {
    bytes: &'a [u8],
    stride: usize,
}

fn on_process(stream: &Stream, st: &mut VideoState) {
    // Only the newest buffer matters; older ones go straight back to the compositor.
    let mut newest = None;
    while let Some(b) = stream.dequeue_buffer() {
        newest = Some(b);
    }
    let Some(mut buffer) = newest else { return };
    if st.flags.is_paused() {
        st.pacer.reset();
        return;
    }
    let Some((layout, w, h)) = st.format else { return };
    if w < 16 || h < 16 {
        return;
    }
    let datas = buffer.datas_mut();
    let Some(first) = datas.first() else { return };
    let chunk = first.chunk();
    // Empty chunk = only the cursor moved (metadata mode) or nothing changed.
    if chunk.size() == 0 || chunk.flags().contains(ChunkFlags::CORRUPTED) {
        return;
    }
    let now = Instant::now();
    if !st.pacer.accept(now, st.flags.interval()) {
        return;
    }

    // Map every plane (normally one; NV12 may come as two).
    let mut maps: Vec<Option<DmaBufMap>> = Vec::with_capacity(datas.len());
    let mut planes: Vec<(*const u8, usize, usize)> = Vec::with_capacity(datas.len()); // (ptr, len, stride)
    for d in datas.iter_mut() {
        let raw = d.as_raw();
        let (offset, stride) = (d.chunk().offset() as usize, d.chunk().stride());
        let (ty, fd, mapoffset, maxsize) = (d.type_(), raw.fd as i32, raw.mapoffset as usize, raw.maxsize as usize);
        let (base, len): (*const u8, usize) = match d.data() {
            Some(bytes) => (bytes.as_ptr(), bytes.len()),
            None if ty == DataType::DmaBuf => {
                if !st.warned_dmabuf {
                    info!("PipeWire delivers DMA-BUF; mapping it on the CPU (linear buffers only)");
                    st.warned_dmabuf = true;
                }
                let Some(map) = DmaBufMap::new(fd, mapoffset + maxsize) else {
                    warn!("cannot mmap the DMA-BUF");
                    return;
                };
                let bytes = &map.bytes()[mapoffset..];
                let p = (bytes.as_ptr(), bytes.len());
                maps.push(Some(map));
                p
            }
            None => {
                debug!("PipeWire buffer of type {ty:?} has no CPU mapping");
                return;
            }
        };
        if offset >= len {
            return;
        }
        planes.push((unsafe { base.add(offset) }, len - offset, stride.max(0) as usize));
    }
    let plane = |i: usize| planes.get(i).map(|&(p, len, stride)| Plane { bytes: unsafe { std::slice::from_raw_parts(p, len) }, stride });

    let (cw, ch) = st.flags.canvas;
    let t = Instant::now();
    let mut nv12 = vec![0u8; (cw * ch * 3 / 2) as usize];
    let ok = match layout {
        Layout::Rgb(order) => match plane(0) {
            Some(p) => {
                let stride = if p.stride > 0 { p.stride } else { w as usize * 4 };
                let need = stride * (h as usize - 1) + w as usize * 4;
                if p.bytes.len() >= need {
                    rgb32_to_nv12_fit(p.bytes, w, h, stride, order, cw, ch, &mut nv12);
                    true
                } else {
                    false
                }
            }
            None => false,
        },
        Layout::Nv12 => match plane(0) {
            Some(y) => {
                let y_stride = if y.stride > 0 { y.stride } else { w as usize };
                let y_need = y_stride * (h as usize - 1) + w as usize;
                // Second plane: its own data, or right after the Y plane in the same one.
                let uv = match plane(1) {
                    Some(p) => Some((p.bytes, if p.stride > 0 { p.stride } else { y_stride })),
                    None => y.bytes.get(y_stride * h as usize..).map(|b| (b, y_stride)),
                };
                match uv {
                    Some((uv, uv_stride)) if y.bytes.len() >= y_need && uv.len() >= uv_stride * (h as usize / 2 - 1) + w as usize => {
                        nv12_fit(y.bytes, y_stride, uv, uv_stride, w, h, cw, ch, &mut nv12);
                        true
                    }
                    _ => false,
                }
            }
            None => false,
        },
    };
    drop(maps);
    if !ok {
        if !st.warned_short {
            warn!("PipeWire buffer smaller than its {w}x{h} {layout:?} format; frames skipped");
            st.warned_short = true;
        }
        return;
    }
    let convert_us = t.elapsed().as_micros() as u64;
    if !st.flags.deliver(CapturedFrame { nv12, width: cw, height: ch, captured: now, convert_us }) {
        st.flags.counters.closed.store(true, Ordering::Relaxed);
    }
}
