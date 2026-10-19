//! libavcodec H.264 encoders through the raw C API (`ffmpeg-sys-next`, bindings generated
//! against the installed FFmpeg 4.4 .. 8.x headers): `h264_vaapi` (NV12 uploaded to a VA
//! surface from a hw frames pool), `h264_nvenc`, `h264_v4l2m2m` and `libx264` (NV12 or
//! YUV420P software frames).
//!
//! Low latency everywhere: no B-frames, CBR (`rc_max_rate = bit_rate`, half a second of
//! VBV), AV_CODEC_FLAG_LOW_DELAY, per-encoder zero-latency options, no global header (so
//! SPS/PPS stay in band), and an I picture with `forced-idr` when a keyframe is requested.
//! Options an older FFmpeg does not know are left unused (logged at debug level), never
//! fatal. BT.709 limited range is signalled to match `convert`.

// Integer types of FFmpeg's #define constants depend on the headers bindgen saw, so some
// `as i32` casts are no-ops on one FFmpeg version and needed on another.
#![allow(clippy::unnecessary_cast)]

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::Once;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use ffmpeg_sys_next as ff;
use log::{debug, info};

use crate::encoder::{EncoderConfig, Kind, RawPacket};

/// FFmpeg error code -> text.
fn av_err(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 128];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn init_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        // FFmpeg prints to stderr; keep it to real errors unless asked for more.
        let level = if std::env::var_os("NANOCAST_FFMPEG_DEBUG").is_some() { ff::AV_LOG_VERBOSE } else { ff::AV_LOG_ERROR };
        ff::av_log_set_level(level as i32);
    });
}

pub(crate) fn has_encoder(name: &str) -> bool {
    let Ok(c) = CString::new(name) else { return false };
    unsafe { !ff::avcodec_find_encoder_by_name(c.as_ptr()).is_null() }
}

/// First DRM render node VA-API can open (`NANOCAST_VAAPI_DEVICE` overrides).
fn vaapi_devices() -> Vec<String> {
    if let Ok(d) = std::env::var("NANOCAST_VAAPI_DEVICE") {
        return vec![d];
    }
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .map(|n| format!("/dev/dri/{n}"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes
}

pub(crate) struct FfEncoder {
    kind: Kind,
    ctx: *mut ff::AVCodecContext,
    /// Software frame the NV12 canvas is copied into.
    frame: *mut ff::AVFrame,
    /// VA-API: surface frame the software frame is uploaded to.
    hw_frame: *mut ff::AVFrame,
    hw_frames: *mut ff::AVBufferRef,
    hw_device: *mut ff::AVBufferRef,
    pkt: *mut ff::AVPacket,
    sw_format: ff::AVPixelFormat,
    width: usize,
    height: usize,
    /// (pts, captured, submitted) of frames inside the encoder.
    pending: VecDeque<(i64, Instant, Instant)>,
    last_pts: i64,
    pub dropped: u64,
}

impl Drop for FfEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.hw_frame);
            ff::av_packet_free(&mut self.pkt);
            ff::av_buffer_unref(&mut self.hw_frames);
            ff::av_buffer_unref(&mut self.hw_device);
        }
    }
}

/// Encoder-private options for low latency, per back end.
fn options(kind: Kind, low_power: bool) -> Vec<(&'static str, &'static str)> {
    match kind {
        Kind::Vaapi => vec![
            ("rc_mode", "CBR"),
            // One frame in flight: the packet comes back from the same send/receive round.
            ("async_depth", "1"),
            ("low_power", if low_power { "1" } else { "0" }),
        ],
        Kind::Nvenc => vec![
            ("preset", "p1"),
            ("tune", "ull"),
            ("rc", "cbr"),
            ("zerolatency", "1"),
            ("delay", "0"),
            ("forced-idr", "1"),
        ],
        Kind::X264 => vec![("preset", "superfast"), ("tune", "zerolatency"), ("forced-idr", "1")],
        Kind::V4l2m2m | Kind::OpenH264 => vec![],
    }
}

impl FfEncoder {
    pub(crate) fn open(kind: Kind, cfg: &EncoderConfig) -> Result<Self> {
        init_logging();
        let mut last_err = anyhow!("no attempt");
        match kind {
            Kind::Vaapi => {
                // Intel Gen 11+ (and some AMD) only expose the low-power entry point.
                for dev in vaapi_devices() {
                    for low_power in [false, true] {
                        match unsafe { Self::open_with(kind, cfg, ff::AVPixelFormat::AV_PIX_FMT_NV12, Some(&dev), low_power) } {
                            Ok(e) => {
                                info!("VA-API on {dev}{}", if low_power { " (low-power entry point)" } else { "" });
                                return Ok(e);
                            }
                            Err(e) => last_err = e,
                        }
                    }
                }
            }
            _ => {
                for fmt in [ff::AVPixelFormat::AV_PIX_FMT_NV12, ff::AVPixelFormat::AV_PIX_FMT_YUV420P] {
                    match unsafe { Self::open_with(kind, cfg, fmt, None, false) } {
                        Ok(e) => return Ok(e),
                        Err(e) => last_err = e,
                    }
                }
            }
        }
        Err(last_err)
    }

    unsafe fn open_with(
        kind: Kind,
        cfg: &EncoderConfig,
        sw_format: ff::AVPixelFormat,
        vaapi_device: Option<&str>,
        low_power: bool,
    ) -> Result<Self> {
        let name = CString::new(kind.name())?;
        let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
        if codec.is_null() {
            bail!("FFmpeg has no {} encoder", kind.name());
        }
        let ctx = ff::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            bail!("avcodec_alloc_context3 failed");
        }
        // From here on `enc` owns everything allocated and frees it on any error.
        let mut enc = Self {
            kind,
            ctx,
            frame: ptr::null_mut(),
            hw_frame: ptr::null_mut(),
            hw_frames: ptr::null_mut(),
            hw_device: ptr::null_mut(),
            pkt: ptr::null_mut(),
            sw_format,
            width: cfg.width as usize,
            height: cfg.height as usize,
            pending: VecDeque::new(),
            last_pts: i64::MIN,
            dropped: 0,
        };
        let c = &mut *ctx;
        c.width = cfg.width as i32;
        c.height = cfg.height as i32;
        c.time_base = ff::AVRational { num: 1, den: 1_000_000 };
        c.framerate = ff::AVRational { num: cfg.fps.max(1) as i32, den: 1 };
        c.bit_rate = cfg.bitrate_bps as i64;
        c.rc_max_rate = cfg.bitrate_bps as i64;
        c.rc_buffer_size = (cfg.bitrate_bps / 2) as i32;
        c.gop_size = cfg.gop_frames.max(1) as i32;
        c.max_b_frames = 0;
        c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
        c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
        c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
        c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        if kind == Kind::X264 {
            c.thread_count = 0; // auto; zerolatency switches x264 to sliced threads
        }

        if let Some(dev) = vaapi_device {
            let dev_c = CString::new(dev)?;
            let r = ff::av_hwdevice_ctx_create(
                &mut enc.hw_device,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                dev_c.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!("VA-API device {dev}: {}", av_err(r));
            }
            enc.hw_frames = ff::av_hwframe_ctx_alloc(enc.hw_device);
            if enc.hw_frames.is_null() {
                bail!("av_hwframe_ctx_alloc failed");
            }
            let frames = &mut *((*enc.hw_frames).data as *mut ff::AVHWFramesContext);
            frames.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            frames.sw_format = sw_format;
            frames.width = cfg.width as i32;
            frames.height = cfg.height as i32;
            frames.initial_pool_size = 8;
            let r = ff::av_hwframe_ctx_init(enc.hw_frames);
            if r < 0 {
                bail!("VA-API surface pool {}x{} NV12: {}", cfg.width, cfg.height, av_err(r));
            }
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            c.hw_frames_ctx = ff::av_buffer_ref(enc.hw_frames);
        } else {
            c.pix_fmt = sw_format;
        }

        let mut dict: *mut ff::AVDictionary = ptr::null_mut();
        for (k, v) in options(kind, low_power) {
            let (k, v) = (CString::new(k)?, CString::new(v)?);
            ff::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0);
        }
        let r = ff::avcodec_open2(ctx, codec, &mut dict);
        // Whatever is left in the dictionary was not recognised by this FFmpeg version.
        let mut e: *mut ff::AVDictionaryEntry = ptr::null_mut();
        let empty = CString::new("")?;
        loop {
            e = ff::av_dict_get(dict, empty.as_ptr(), e, ff::AV_DICT_IGNORE_SUFFIX as i32);
            if e.is_null() {
                break;
            }
            debug!(
                "{}: option {}={} not supported by this FFmpeg",
                kind.name(),
                CStr::from_ptr((*e).key).to_string_lossy(),
                CStr::from_ptr((*e).value).to_string_lossy()
            );
        }
        ff::av_dict_free(&mut dict);
        if r < 0 {
            bail!("{} ({}x{}, {:?}): {}", kind.name(), cfg.width, cfg.height, sw_format, av_err(r));
        }

        enc.frame = ff::av_frame_alloc();
        enc.pkt = ff::av_packet_alloc();
        if enc.frame.is_null() || enc.pkt.is_null() {
            bail!("out of memory");
        }
        let f = &mut *enc.frame;
        f.format = sw_format as i32;
        f.width = cfg.width as i32;
        f.height = cfg.height as i32;
        let r = ff::av_frame_get_buffer(enc.frame, 0);
        if r < 0 {
            bail!("av_frame_get_buffer: {}", av_err(r));
        }
        if !enc.hw_frames.is_null() {
            enc.hw_frame = ff::av_frame_alloc();
            if enc.hw_frame.is_null() {
                bail!("out of memory");
            }
        }
        Ok(enc)
    }

    /// libx264 and NVENC pick up a new `bit_rate` before the next frame (x264_encoder_reconfig,
    /// NVENC dynamic bitrate); VA-API and V4L2 M2M only read it when opened.
    pub(crate) fn set_bitrate(&mut self, bitrate_bps: u32) -> bool {
        if !matches!(self.kind, Kind::X264 | Kind::Nvenc) {
            return false;
        }
        unsafe {
            let c = &mut *self.ctx;
            c.bit_rate = bitrate_bps as i64;
            c.rc_max_rate = bitrate_bps as i64;
            c.rc_buffer_size = (bitrate_bps / 2) as i32;
        }
        true
    }

    /// SPS/PPS when the encoder put them in extradata (not with in-band headers).
    pub(crate) fn extradata(&self) -> Vec<u8> {
        unsafe {
            let c = &*self.ctx;
            if c.extradata.is_null() || c.extradata_size <= 0 {
                return Vec::new();
            }
            std::slice::from_raw_parts(c.extradata, c.extradata_size as usize).to_vec()
        }
    }

    /// Copies the NV12 canvas into the encoder's frame layout.
    unsafe fn fill_frame(&mut self, nv12: &[u8]) -> Result<()> {
        let r = ff::av_frame_make_writable(self.frame);
        if r < 0 {
            bail!("av_frame_make_writable: {}", av_err(r));
        }
        let (w, h) = (self.width, self.height);
        if nv12.len() < w * h * 3 / 2 {
            bail!("frame of {} bytes, expected {}", nv12.len(), w * h * 3 / 2);
        }
        let f = &mut *self.frame;
        let (y, uv) = nv12.split_at(w * h);
        for row in 0..h {
            let dst = f.data[0].add(row * f.linesize[0] as usize);
            ptr::copy_nonoverlapping(y.as_ptr().add(row * w), dst, w);
        }
        if self.sw_format == ff::AVPixelFormat::AV_PIX_FMT_NV12 {
            for row in 0..h / 2 {
                let dst = f.data[1].add(row * f.linesize[1] as usize);
                ptr::copy_nonoverlapping(uv.as_ptr().add(row * w), dst, w);
            }
        } else {
            for row in 0..h / 2 {
                let src = &uv[row * w..row * w + w];
                let u = std::slice::from_raw_parts_mut(f.data[1].add(row * f.linesize[1] as usize), w / 2);
                let v = std::slice::from_raw_parts_mut(f.data[2].add(row * f.linesize[2] as usize), w / 2);
                for x in 0..w / 2 {
                    u[x] = src[2 * x];
                    v[x] = src[2 * x + 1];
                }
            }
        }
        Ok(())
    }

    pub(crate) fn submit(&mut self, nv12: &[u8], pts_us: i64, captured: Instant, force_keyframe: bool) -> Result<()> {
        // Software encoders (x264) do the work inside avcodec_send_frame: time from here.
        let submitted = Instant::now();
        unsafe {
            self.fill_frame(nv12)?;
            let pts = if pts_us <= self.last_pts { self.last_pts + 1 } else { pts_us };
            self.last_pts = pts;
            let pict_type =
                if force_keyframe { ff::AVPictureType::AV_PICTURE_TYPE_I } else { ff::AVPictureType::AV_PICTURE_TYPE_NONE };
            (*self.frame).pts = pts;
            (*self.frame).pict_type = pict_type;

            let input = if self.hw_frame.is_null() {
                self.frame
            } else {
                ff::av_frame_unref(self.hw_frame);
                let r = ff::av_hwframe_get_buffer(self.hw_frames, self.hw_frame, 0);
                if r < 0 {
                    bail!("VA-API surface: {}", av_err(r));
                }
                let r = ff::av_hwframe_transfer_data(self.hw_frame, self.frame, 0);
                if r < 0 {
                    bail!("VA-API upload: {}", av_err(r));
                }
                (*self.hw_frame).pts = pts;
                (*self.hw_frame).pict_type = pict_type;
                self.hw_frame
            };

            let r = ff::avcodec_send_frame(self.ctx, input);
            if r == ff::AVERROR(libc::EAGAIN) {
                // Encoder queue full (V4L2 M2M can lag): drop this frame, keep the keyframe wish.
                self.dropped += 1;
                return Ok(());
            }
            if r < 0 {
                bail!("avcodec_send_frame: {}", av_err(r));
            }
            self.pending.push_back((pts, captured, submitted));
            // Bound the bookkeeping if an encoder never returns some frames.
            while self.pending.len() > 64 {
                self.pending.pop_front();
            }
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self, out: &mut Vec<RawPacket>) -> Result<()> {
        unsafe {
            loop {
                let r = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if r == ff::AVERROR(libc::EAGAIN) || r == ff::AVERROR_EOF {
                    return Ok(());
                }
                if r < 0 {
                    bail!("avcodec_receive_packet: {}", av_err(r));
                }
                let p = &*self.pkt;
                let data = if p.data.is_null() || p.size <= 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(p.data, p.size as usize).to_vec()
                };
                // Match the packet to its input frame by pts (no reordering: no B-frames).
                let mut times = None;
                while let Some(&(pts, captured, submitted)) = self.pending.front() {
                    if p.pts != ff::AV_NOPTS_VALUE && pts > p.pts {
                        break;
                    }
                    self.pending.pop_front();
                    times = Some((captured, submitted));
                    if p.pts == ff::AV_NOPTS_VALUE || pts == p.pts {
                        break;
                    }
                }
                ff::av_packet_unref(self.pkt);
                let (captured, submitted) = times.unwrap_or((Instant::now(), Instant::now()));
                if !data.is_empty() {
                    out.push(RawPacket { data, captured, submitted });
                }
            }
        }
    }
}
