//! Media Foundation hardware H.264 encoder (asynchronous MFT).
//! Works with the Intel / NVIDIA / AMD vendor MFTs without any vendor SDK.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Frames between keyframes (large: keyframes come on request instead).
    pub gop_frames: u32,
    pub encoder_index: usize,
}

pub struct EncodedFrame {
    pub pts_hns: i64,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Wall-clock time from ProcessInput to ProcessOutput for this frame.
    pub encode_latency_us: u64,
    /// The caller-supplied capture instant, echoed back for end-to-end stats.
    pub capture_instant: Instant,
}

struct Inflight {
    pts_hns: i64,
    submitted: Instant,
    capture_instant: Instant,
}

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    provides_samples: bool,
    output_size: u32,
    frame_duration_hns: i64,
    inflight: VecDeque<Inflight>,
    pending_need_input: u32,
    debug: bool,
    pub name: String,
}

fn var_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

fn var_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal = if v { VARIANT_TRUE } else { Default::default() };
    }
    var
}

/// Lists hardware H.264 encoder MFTs, sorted by Media Foundation merit.
pub fn list_hw_h264_encoders() -> Result<Vec<(String, IMFActivate)>> {
    unsafe {
        let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&out_info),
            &mut pp,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let mut result = Vec::new();
        if !pp.is_null() {
            let slice = std::slice::from_raw_parts(pp, count as usize);
            for act in slice.iter().flatten() {
                let mut name = PWSTR::null();
                let mut len = 0u32;
                let friendly = if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
                    let s = name.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(name.0 as _));
                    s
                } else {
                    "<unnamed>".to_string()
                };
                result.push((friendly, act.clone()));
            }
            CoTaskMemFree(Some(pp as _));
        }
        Ok(result)
    }
}

impl MfEncoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self> {
        let encoders = list_hw_h264_encoders()?;
        if encoders.is_empty() {
            bail!("no hardware H.264 encoder found (Media Foundation)");
        }
        let (name, activate) =
            encoders.get(cfg.encoder_index).ok_or_else(|| anyhow!("encoder index out of range"))?;
        unsafe {
            let transform: IMFTransform = match activate.ActivateObject() {
                Ok(t) => t,
                Err(e) => {
                    // Some vendor MFTs (seen with NVIDIA) fail ActivateObject but
                    // can be created directly from their CLSID.
                    eprintln!("  ActivateObject failed ({e}); trying CoCreateInstance");
                    let clsid = activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).context("MFT CLSID")?;
                    CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER).context("CoCreateInstance MFT")?
                }
            };

            // Async MFT handshake.
            let attrs = transform.GetAttributes().context("GetAttributes")?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if !is_async {
                bail!("{name}: not an async MFT; hardware encoders are expected to be async");
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // Codec properties. Failures are logged, not fatal: vendors differ.
            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                let set = |guid: &GUID, v: VARIANT, label: &str| {
                    if let Err(e) = codec.SetValue(guid, &v) {
                        eprintln!("  [codecapi] {label}: not supported ({e})");
                    }
                };
                set(&CODECAPI_AVEncCommonRateControlMode, var_u32(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(cfg.bitrate_bps), "MeanBitRate");
                set(&CODECAPI_AVEncCommonRealTime, var_bool(true), "RealTime");
                set(&CODECAPI_AVLowLatencyMode, var_bool(true), "LowLatencyMode");
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0), "BFrames=0");
                set(&CODECAPI_AVEncMPVGOPSize, var_u32(cfg.gop_frames), "GOP");
                set(&CODECAPI_AVEncVideoMaxNumRefFrame, var_u32(1), "MaxRef=1");
                // Gradual (rolling) intra refresh instead of big IDR frames; few MFTs
                // expose it through Media Foundation, so this is best effort.
                set(&CODECAPI_AVEncVideoGradualIntraRefresh, var_u32(1), "GradualIntraRefresh");
            }

            // Output type first (encoders want it before the input type).
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((cfg.width as u64) << 32) | cfg.height as u64)?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(0, &out_type, 0).context("SetOutputType H264")?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, ((cfg.width as u64) << 32) | cfg.height as u64)?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width)?;
            transform.SetInputType(0, &in_type, 0).context("SetInputType NV12")?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            if std::env::var_os("NANOCAST_DEBUG").is_some() {
                eprintln!("  [mf] output stream flags=0x{:08x} cbSize={} provides_samples={provides_samples}", info.dwFlags, info.cbSize);
            }

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let events: IMFMediaEventGenerator = transform.cast().context("IMFMediaEventGenerator")?;

            Ok(Self {
                transform,
                events,
                codec,
                provides_samples,
                output_size: info.cbSize.max(cfg.width * cfg.height),
                frame_duration_hns: 10_000_000 / cfg.fps as i64,
                inflight: VecDeque::new(),
                pending_need_input: 0,
                debug: std::env::var_os("NANOCAST_DEBUG").is_some(),
                name: name.clone(),
            })
        }
    }

    /// Blocks until the encoder wants a frame, delivering any output produced meanwhile.
    pub fn wait_need_input(&mut self, mut on_output: impl FnMut(EncodedFrame)) -> Result<()> {
        if self.pending_need_input > 0 {
            self.pending_need_input -= 1;
            return Ok(());
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let Some(ty) = self.next_event()? else {
                if Instant::now() > deadline {
                    bail!("encoder sent no NeedInput for 3 s (inflight={})", self.inflight.len());
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
                continue;
            };
            if ty == METransformNeedInput.0 {
                return Ok(());
            } else if ty == METransformHaveOutput.0 {
                if let Some(f) = self.pull_output()? {
                    on_output(f);
                }
            }
        }
    }

    /// Drains any output events already queued, without blocking.
    pub fn poll_output(&mut self, mut on_output: impl FnMut(EncodedFrame)) -> Result<()> {
        while let Some(ty) = self.next_event()? {
            if ty == METransformHaveOutput.0 {
                if let Some(f) = self.pull_output()? {
                    on_output(f);
                }
            } else if ty == METransformNeedInput.0 {
                // Consumed here; wait_need_input() must not miss it.
                self.pending_need_input += 1;
            }
        }
        Ok(())
    }

    /// Non-blocking: returns the next event type, or None when the queue is empty.
    fn next_event(&mut self) -> Result<Option<i32>> {
        let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(ev) => ev,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
            Err(e) => return Err(e).context("GetEvent"),
        };
        let ty = unsafe { ev.GetType()? } as i32;
        if self.debug {
            let status = unsafe { ev.GetStatus() }.map(|h| h.0).unwrap_or(0);
            eprintln!("  [mf-event] type={ty} status=0x{status:08x}");
        }
        if ty == MEError.0 {
            let status = unsafe { ev.GetStatus() }.map(|h| h.0).unwrap_or(0);
            bail!("encoder reported MEError (0x{status:08x})");
        }
        Ok(Some(ty))
    }

    /// Asks the encoder to make the next frame a keyframe (receiver recovery).
    pub fn request_keyframe(&mut self) {
        if let Some(codec) = &self.codec {
            if let Err(e) = unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1)) } {
                eprintln!("  [codecapi] ForceKeyFrame failed: {e}");
            }
        }
    }

    pub fn submit(&mut self, nv12: &[u8], pts_hns: i64, capture_instant: Instant) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_hns)?;
            sample.SetSampleDuration(self.frame_duration_hns)?;
            self.inflight.push_back(Inflight { pts_hns, submitted: Instant::now(), capture_instant });
            self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
        }
        Ok(())
    }

    fn pull_output(&mut self) -> Result<Option<EncodedFrame>> {
        unsafe {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            if !self.provides_samples {
                let buffer = MFCreateMemoryBuffer(self.output_size)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                out.pSample = ManuallyDrop::new(Some(sample));
            }
            let mut status = 0u32;
            let r = self.transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
            let sample = ManuallyDrop::take(&mut out.pSample);
            ManuallyDrop::drop(&mut out.pEvents);
            if self.debug {
                eprintln!("  [mf-output] hr={:?} status=0x{status:08x} buf_status=0x{:08x} sample={}", r.as_ref().map(|_| ()).map_err(|e| e.code().0), out.dwStatus, sample.is_some());
            }
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Hardware encoders renegotiate the output type once at start.
                    let new_type = self.transform.GetOutputAvailableType(0, 0).context("GetOutputAvailableType")?;
                    self.transform.SetOutputType(0, &new_type, 0).context("SetOutputType (stream change)")?;
                    if self.debug {
                        eprintln!("  [mf-output] stream change handled, output type reset");
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let Some(sample) = sample else { return Ok(None) };
            let now = Instant::now();
            let pts_hns = sample.GetSampleTime().unwrap_or(-1);
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;

            // Outputs arrive in submission order with B-frames disabled.
            let inflight = match self.inflight.iter().position(|f| f.pts_hns == pts_hns) {
                Some(i) => {
                    let f = self.inflight.remove(i).unwrap();
                    self.inflight.drain(..i);
                    f
                }
                None => self.inflight.pop_front().unwrap_or(Inflight { pts_hns, submitted: now, capture_instant: now }),
            };
            Ok(Some(EncodedFrame {
                pts_hns,
                keyframe,
                data,
                encode_latency_us: now.duration_since(inflight.submitted).as_micros() as u64,
                capture_instant: inflight.capture_instant,
            }))
        }
    }
}
