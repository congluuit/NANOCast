//! H.264 with VideoToolbox (`VTCompressionSession`), tuned like the Windows MF encoder:
//! real-time, no frame reordering (no B-frames), average bit rate plus a 1 s data-rate
//! ceiling (near-CBR), Main profile with CABAC, keyframes every `gop_frames` or on request
//! (`kVTEncodeFrameOptionKey_ForceKeyFrame`).
//!
//! The session first asks for the low-latency rate controller
//! (`EnableLowLatencyRateControl`, macOS 11.3+, Apple Silicon and recent Intel); if the
//! machine refuses it, a normal hardware-preferred session is opened instead.
//!
//! Output arrives on a VideoToolbox thread (the output callback); it is converted from
//! AVCC to Annex B with SPS/PPS before every keyframe (`crate::annexb`) and queued until
//! the session loop calls `poll_output`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use log::{info, warn};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMTimeInvalid, kCMVideoCodecType_H264, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_H264EntropyMode, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTH264EntropyMode_CABAC, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionCopyProperty, VTSessionSetProperty,
};

use crate::annexb::avcc_to_annexb;
use crate::capture::PixelBuffer;

/// The one H.264 encoder a Mac has (shown in the GUI like the Windows MFT list).
pub const ENCODER_NAME: &str = "Apple VideoToolbox H.264";

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Keyframe interval in frames.
    pub gop_frames: u32,
}

pub struct EncodedFrame {
    pub pts_hns: i64,
    pub keyframe: bool,
    /// H.264 Annex B access unit.
    pub data: Vec<u8>,
    /// Submit -> output callback.
    pub encode_latency_us: u64,
    pub capture_instant: Instant,
}

/// Names for the GUI's encoder list. VideoToolbox picks the hardware block itself.
pub fn hw_encoder_names() -> Result<Vec<String>> {
    Ok(vec![ENCODER_NAME.to_string()])
}

/// Per-frame data handed through VideoToolbox (`sourceFrameRefcon`).
struct FrameMeta {
    pts_hns: i64,
    submitted: Instant,
    captured: Instant,
}

/// Shared between the encoder and the VideoToolbox output callback.
#[derive(Default)]
struct Shared {
    out: Mutex<VecDeque<EncodedFrame>>,
    error: Mutex<Option<String>>,
    dropped: AtomicU64,
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// `Arc::into_raw` of the callback state; reclaimed in `Drop` after `invalidate`.
    shared: *const Shared,
    force_keyframe: bool,
    pub name: String,
    pub hardware: bool,
    pub low_latency: bool,
    fps: u32,
}

impl VtEncoder {
    /// Low-latency rate control first, then a normal session.
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        match Self::new(cfg, true) {
            Ok(e) => Ok(e),
            Err(e) => {
                warn!("VideoToolbox low-latency session unavailable ({e:#}); using the normal rate controller");
                Self::new(cfg, false)
            }
        }
    }

    pub fn new(cfg: &EncoderConfig, low_latency: bool) -> Result<Self> {
        let shared = Arc::into_raw(Arc::new(Shared::default()));
        let session = unsafe { create_session(cfg, low_latency, shared) };
        let session = match session {
            Ok(s) => s,
            Err(e) => {
                drop(unsafe { Arc::from_raw(shared) });
                return Err(e);
            }
        };
        let mut enc = Self { session, shared, force_keyframe: false, name: String::new(), hardware: false, low_latency, fps: cfg.fps.max(1) };
        unsafe { enc.configure(cfg)? };
        let status = unsafe { enc.session.prepare_to_encode_frames() };
        if status != 0 {
            bail!("VTCompressionSessionPrepareToEncodeFrames failed ({status})");
        }
        enc.hardware = unsafe { copy_bool(&enc.session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder) }.unwrap_or(false);
        enc.name = format!(
            "{ENCODER_NAME} ({}{})",
            if enc.hardware { "hardware" } else { "software" },
            if low_latency { ", low-latency" } else { "" }
        );
        info!("encoder: {} {}x{} @ {} fps, {} kbps, GOP {}", enc.name, cfg.width, cfg.height, cfg.fps, cfg.bitrate_bps / 1000, cfg.gop_frames);
        Ok(enc)
    }

    unsafe fn configure(&self, cfg: &EncoderConfig) -> Result<()> {
        let s = &self.session;
        let fps = cfg.fps.max(1);
        // Fatal if refused: without these the stream is not low-latency at all.
        set(s, kVTCompressionPropertyKey_RealTime, CFBoolean::new(true)).context("RealTime")?;
        set(s, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false)).context("AllowFrameReordering")?;
        set(s, kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(cfg.bitrate_bps as i64)).context("AverageBitRate")?;
        // Best effort (some are fixed by the low-latency controller).
        let soft = |what: &str, r: Result<()>| {
            if let Err(e) = r {
                warn!("VideoToolbox {what} not applied: {e:#}");
            }
        };
        soft("MaxKeyFrameInterval", set(s, kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(cfg.gop_frames.max(1) as i32)));
        soft(
            "MaxKeyFrameIntervalDuration",
            set(s, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_f64(cfg.gop_frames.max(1) as f64 / fps as f64)),
        );
        soft("ExpectedFrameRate", set(s, kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(fps as i32)));
        // Ceiling: 1.5x the average over any 1 s window (bytes, seconds) keeps it near CBR
        // so a busy scene cannot burst past what the Wi-Fi link carries.
        let bytes = CFNumber::new_i64(cfg.bitrate_bps as i64 / 8 * 3 / 2);
        let secs = CFNumber::new_f64(1.0);
        let limits = CFArray::<CFNumber>::from_objects(&[&*bytes, &*secs]);
        soft("DataRateLimits", set(s, kVTCompressionPropertyKey_DataRateLimits, &limits));
        soft("ProfileLevel", set(s, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel));
        soft("H264EntropyMode", set(s, kVTCompressionPropertyKey_H264EntropyMode, kVTH264EntropyMode_CABAC));
        Ok(())
    }

    fn shared(&self) -> &Shared {
        unsafe { &*self.shared }
    }

    /// The next submitted frame becomes an IDR (with SPS/PPS in front).
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Frames VideoToolbox decided to drop (rate control), for stats.
    pub fn dropped(&self) -> u64 {
        self.shared().dropped.load(Ordering::Relaxed)
    }

    /// Queues one NV12 frame. Returns quickly; output comes through `poll_output`.
    pub fn submit(&mut self, pixels: &PixelBuffer, pts_hns: i64, captured: Instant) -> Result<()> {
        self.check_error()?;
        let meta = Box::into_raw(Box::new(FrameMeta { pts_hns, submitted: Instant::now(), captured }));
        let pts = unsafe { CMTime::new(pts_hns / 10, 1_000_000) };
        let duration = unsafe { CMTime::new(1, self.fps as i32) };
        let force = std::mem::take(&mut self.force_keyframe);
        let props = force.then(|| {
            let key: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)])
        });
        let mut flags = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(&pixels.0, pts, duration, props.as_deref().map(|d| d.as_opaque()), meta.cast(), &mut flags)
        };
        if status != 0 {
            // Whether VideoToolbox still calls back for a frame it refused is not
            // documented, so `meta` is leaked (a few bytes, and the cast ends on this
            // error) rather than risking a double free.
            self.force_keyframe |= force;
            bail!("VTCompressionSessionEncodeFrame failed ({status})");
        }
        Ok(())
    }

    /// Hands every finished frame to `on_output`.
    pub fn poll_output(&mut self, mut on_output: impl FnMut(EncodedFrame)) -> Result<()> {
        self.check_error()?;
        let ready: Vec<EncodedFrame> = self.shared().out.lock().unwrap().drain(..).collect();
        ready.into_iter().for_each(&mut on_output);
        Ok(())
    }

    fn check_error(&self) -> Result<()> {
        match self.shared().error.lock().unwrap().take() {
            Some(e) => Err(anyhow!(e)),
            None => Ok(()),
        }
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe {
            // Flush: every pending frame gets its callback (and its FrameMeta freed) now;
            // after invalidate no callback runs, so `shared` can go.
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            drop(Arc::from_raw(self.shared));
        }
    }
}

unsafe fn create_session(cfg: &EncoderConfig, low_latency: bool, shared: *const Shared) -> Result<CFRetained<VTCompressionSession>> {
    let hw: &CFString = kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder;
    let ll: &CFString = kVTVideoEncoderSpecification_EnableLowLatencyRateControl;
    let yes = CFBoolean::new(true);
    let spec = if low_latency {
        CFDictionary::<CFString, CFBoolean>::from_slices(&[hw, ll], &[yes, yes])
    } else {
        CFDictionary::<CFString, CFBoolean>::from_slices(&[hw], &[yes])
    };
    let mut out: *mut VTCompressionSession = std::ptr::null_mut();
    let status = VTCompressionSession::create(
        None,
        cfg.width as i32,
        cfg.height as i32,
        kCMVideoCodecType_H264,
        Some(spec.as_opaque()),
        None,
        None,
        Some(on_encoded),
        shared as *mut c_void,
        NonNull::from(&mut out),
    );
    if status != 0 {
        bail!("VTCompressionSessionCreate failed ({status})");
    }
    let session = NonNull::new(out).ok_or_else(|| anyhow!("VTCompressionSessionCreate returned no session"))?;
    Ok(CFRetained::from_raw(session))
}

unsafe fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> Result<()> {
    let status = VTSessionSetProperty(session, key, Some(value));
    if status != 0 {
        bail!("VTSessionSetProperty({key}) failed ({status})");
    }
    Ok(())
}

unsafe fn copy_bool(session: &VTCompressionSession, key: &CFString) -> Option<bool> {
    let mut value: *const CFBoolean = std::ptr::null();
    let status = VTSessionCopyProperty(session, key, None, (&mut value as *mut *const CFBoolean).cast());
    let value = CFRetained::from_raw(NonNull::new(value as *mut CFBoolean)?);
    (status == 0).then(|| value.as_bool())
}

/// VideoToolbox output callback (runs on a VideoToolbox thread).
unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let shared = &*(refcon as *const Shared);
    let meta = Box::from_raw(frame_refcon as *mut FrameMeta);
    if status != 0 {
        *shared.error.lock().unwrap() = Some(format!("VideoToolbox encode error ({status})"));
        return;
    }
    let Some(sample) = sample.as_ref().filter(|_| !flags.contains(VTEncodeInfoFlags::FrameDropped)) else {
        shared.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    match annexb_from_sample(sample) {
        Ok((data, keyframe)) => shared.out.lock().unwrap().push_back(EncodedFrame {
            pts_hns: meta.pts_hns,
            keyframe,
            data,
            encode_latency_us: meta.submitted.elapsed().as_micros() as u64,
            capture_instant: meta.captured,
        }),
        Err(e) => *shared.error.lock().unwrap() = Some(format!("{e:#}")),
    }
}

/// Copies the AVCC payload out of the sample buffer and converts it to Annex B.
unsafe fn annexb_from_sample(sample: &CMSampleBuffer) -> Result<(Vec<u8>, bool)> {
    let block = sample.data_buffer().context("encoded sample has no data")?;
    let len = block.data_length();
    let mut avcc = vec![0u8; len];
    if len > 0 {
        let status = block.copy_data_bytes(0, len, NonNull::new_unchecked(avcc.as_mut_ptr()).cast());
        if status != 0 {
            bail!("CMBlockBufferCopyDataBytes failed ({status})");
        }
    }
    let desc = sample.format_description().context("encoded sample has no format description")?;
    let (param_sets, nal_len) = parameter_sets(&desc)?;
    let sets: Vec<&[u8]> = param_sets.iter().map(|p| p.as_slice()).collect();
    let mut out = Vec::with_capacity(len + 128);
    let keyframe = avcc_to_annexb(&avcc, nal_len, &sets, &mut out).context("malformed AVCC from VideoToolbox")?;
    Ok((out, keyframe))
}

/// SPS, PPS (and any further parameter sets) plus the AVCC length-prefix size.
unsafe fn parameter_sets(desc: &CMFormatDescription) -> Result<(Vec<Vec<u8>>, usize)> {
    let (mut count, mut nal_len) = (0usize, 0i32);
    let mut ptr: *const u8 = std::ptr::null();
    let mut size = 0usize;
    let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, 0, &mut ptr, &mut size, &mut count, &mut nal_len);
    if status != 0 {
        bail!("CMVideoFormatDescriptionGetH264ParameterSetAtIndex failed ({status})");
    }
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        if i > 0 {
            let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc,
                i,
                &mut ptr,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if status != 0 {
                bail!("parameter set {i} unavailable ({status})");
            }
        }
        if !ptr.is_null() && size > 0 {
            sets.push(std::slice::from_raw_parts(ptr, size).to_vec());
        }
    }
    Ok((sets, nal_len.max(1) as usize))
}
