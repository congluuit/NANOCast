//! ScreenCaptureKit capture of one display or one window, delivered as NV12 CVPixelBuffers
//! already scaled to the fixed canvas (encoder) size on the GPU: no CPU conversion, the
//! buffer goes straight to VideoToolbox.
//!
//! Pacing: `minimumFrameInterval = 1/fps`. Like Windows Graphics Capture, SCK only sends
//! a frame when something changed (idle frames carry no picture and are skipped).
//! `queueDepth` is small (3): the pipeline holds at most one buffer in the channel and
//! one in the encoder, so capture never stalls on us but latency stays low.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use log::{info, warn};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};

use crate::audio::AudioTap;
use crate::sources;

/// Frames ScreenCaptureKit may have in flight; see the module docs.
pub const QUEUE_DEPTH: isize = 3;

/// What to capture (same shape as `nanocast_win::capture::Source`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// One-based display index (see `sources::list_monitors`).
    Monitor(usize),
    /// CGWindowID of an application window (see `sources::list_windows`).
    Window(isize),
}

/// A retained NV12 CVPixelBuffer from ScreenCaptureKit's pool. CoreVideo buffers are
/// reference counted and safe to hand between threads; we only read them.
pub struct PixelBuffer(pub CFRetained<CVImageBuffer>);
unsafe impl Send for PixelBuffer {}

pub struct CapturedFrame {
    pub pixels: PixelBuffer,
    pub width: u32,
    pub height: u32,
    pub captured: Instant,
    /// Always 0: scaling and NV12 conversion happen inside ScreenCaptureKit.
    pub convert_us: u64,
}

#[derive(Default)]
pub struct CaptureCounters {
    pub captured: AtomicUsize,
    /// Frames dropped because the encoder was still busy.
    pub dropped: AtomicUsize,
    /// Set when the stream stopped by itself (window closed, display gone, permission revoked).
    pub closed: AtomicBool,
}

pub struct CaptureFlags {
    pub tx: SyncSender<CapturedFrame>,
    /// Minimum spacing between frames (`minimumFrameInterval`).
    pub min_interval: Duration,
    /// Output (encoder) size; ScreenCaptureKit scales to fit, keeping the aspect ratio.
    pub canvas: (u32, u32),
    pub counters: Arc<CaptureCounters>,
    /// System sound goes here when set (macOS 13+; ignored on older systems).
    pub audio: Option<Arc<AudioTap>>,
}

struct OutputIvars {
    tx: SyncSender<CapturedFrame>,
    counters: Arc<CaptureCounters>,
    audio: Option<Arc<AudioTap>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `StreamOutput` has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "NanoCastStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                self.on_video(sample);
            } else if kind == SCStreamOutputType::Audio {
                if let Some(tap) = &self.ivars().audio {
                    tap.push(sample);
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            warn!("capture stopped by the system: {}", error.localizedDescription());
            self.ivars().counters.closed.store(true, Ordering::Relaxed);
        }
    }
);

impl StreamOutput {
    fn new(ivars: OutputIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    fn on_video(&self, sample: &CMSampleBuffer) {
        let iv = self.ivars();
        if !unsafe { sample.is_valid() } || frame_status(sample).is_some_and(|s| s != SCFrameStatus::Complete) {
            return;
        }
        // Idle frames (nothing changed) carry no image buffer.
        let Some(image) = (unsafe { sample.image_buffer() }) else { return };
        let (width, height) = (CVPixelBufferGetWidth(&image) as u32, CVPixelBufferGetHeight(&image) as u32);
        iv.counters.captured.fetch_add(1, Ordering::Relaxed);
        let frame = CapturedFrame { pixels: PixelBuffer(image), width, height, captured: Instant::now(), convert_us: 0 };
        match iv.tx.try_send(frame) {
            Ok(()) => {}
            // Encoder busy: drop this frame (the buffer goes back to SCK's pool).
            Err(TrySendError::Full(_)) => {
                iv.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => iv.counters.closed.store(true, Ordering::Relaxed),
        }
    }
}

/// `SCStreamFrameInfoStatus` from the first sample attachment dictionary.
fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: SCK documents the attachments as an array of dictionaries keyed by
    // SCStreamFrameInfo (NSString, toll-free bridged to CFString).
    let attachments: CFRetained<CFArray<CFDictionary>> = unsafe { CFRetained::cast_unchecked(attachments) };
    let dict = attachments.get(0)?;
    let dict: CFRetained<CFDictionary<CFString, CFNumber>> = unsafe { CFRetained::cast_unchecked(dict) };
    let key: &NSString = unsafe { SCStreamFrameInfoStatus };
    let key: &CFString = unsafe { &*(key as *const NSString as *const CFString) };
    let status = dict.get(key)?.as_i64()?;
    Some(SCFrameStatus(status as isize))
}

/// A running capture. Must be stopped (or dropped) on the thread that owns the session.
pub struct Handle {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    _queues: Vec<DispatchRetained<DispatchQueue>>,
    counters: Arc<CaptureCounters>,
    stopped: bool,
}

impl Handle {
    /// True once the system stopped the stream (window closed, display unplugged...).
    pub fn is_finished(&self) -> bool {
        self.counters.closed.load(Ordering::Relaxed)
    }

    /// Stops the stream and waits (up to 2 s) for ScreenCaptureKit to confirm.
    pub fn stop(mut self) -> Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<()> {
        if std::mem::replace(&mut self.stopped, true) {
            return Ok(());
        }
        let (tx, rx) = sync_channel::<Option<String>>(1);
        let done = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.try_send(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()));
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(None) => Ok(()),
            // Already stopped by the system (e.g. the window closed): nothing to release.
            Ok(Some(_)) if self.is_finished() => Ok(()),
            Ok(Some(e)) => Err(anyhow!("stopCapture: {e}")),
            Err(_) => Err(anyhow!("stopCapture did not complete within 2 s")),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Err(e) = self.stop_inner() {
            warn!("capture stop: {e:#}");
        }
    }
}

/// Native pixel size of the source (displays: backing pixels; windows: points x scale
/// of the main display, only used for logging since windows are fitted into the canvas).
pub fn source_size(source: &Source) -> Result<(u32, u32)> {
    let content = sources::shareable_content()?;
    match *source {
        Source::Monitor(index) => {
            let displays = sources::displays(&content);
            let d = displays.get(index.wrapping_sub(1)).ok_or_else(|| anyhow!("display {index} not found"))?;
            Ok(sources::display_pixels(d))
        }
        Source::Window(id) => {
            let w = sources::find_window(&content, id as u32).ok_or_else(|| anyhow!("window {id} not found"))?;
            let f = unsafe { w.frame() };
            Ok((f.size.width as u32, f.size.height as u32))
        }
    }
}

pub fn start(source: &Source, flags: CaptureFlags) -> Result<Handle> {
    let content = sources::shareable_content()?;
    let filter = match *source {
        Source::Monitor(index) => {
            let displays = sources::displays(&content);
            let d = displays.get(index.wrapping_sub(1)).ok_or_else(|| anyhow!("display {index} not found"))?;
            unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), d, &NSArray::new()) }
        }
        Source::Window(id) => {
            let w = sources::find_window(&content, id as u32)
                .ok_or_else(|| anyhow!("window {id} not found (closed, or no Screen Recording permission)"))?;
            unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w) }
        }
    };

    let fps = (1_000_000 / flags.min_interval.as_micros().max(1)) as i32;
    let audio = flags.audio.clone().filter(|_| sources::audio_supported());
    let config = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(flags.canvas.0 as usize);
        c.setHeight(flags.canvas.1 as usize);
        c.setMinimumFrameInterval(CMTime::new(1, fps.max(1)));
        c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        c.setQueueDepth(QUEUE_DEPTH);
        c.setShowsCursor(true);
        c.setScalesToFit(true);
        if sources::os_at_least(14, 0) {
            // Older systems already letterbox when scaling; the property itself is 14.0+.
            c.setPreservesAspectRatio(true);
        }
        if audio.is_some() {
            c.setCapturesAudio(true);
            c.setSampleRate(nanocast_core::protocol::AUDIO_SAMPLE_RATE as isize);
            c.setChannelCount(nanocast_core::protocol::AUDIO_CHANNELS as isize);
            // Never send our own sounds back (and no feedback if NANOCast ever plays any).
            c.setExcludesCurrentProcessAudio(true);
        }
        c
    };

    let counters = flags.counters.clone();
    let output = StreamOutput::new(OutputIvars { tx: flags.tx, counters: counters.clone(), audio: audio.clone() });
    let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
    let stream =
        unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate)) };

    let video_queue = DispatchQueue::new("nanocast.capture.video", None);
    let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
    unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Screen, Some(&video_queue)) }
        .map_err(|e| anyhow!("addStreamOutput(screen): {}", e.localizedDescription()))?;
    let mut queues = vec![video_queue];
    if audio.is_some() {
        let audio_queue = DispatchQueue::new("nanocast.capture.audio", None);
        unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Audio, Some(&audio_queue)) }
            .map_err(|e| anyhow!("addStreamOutput(audio): {}", e.localizedDescription()))?;
        queues.push(audio_queue);
    }

    let (tx, rx) = sync_channel::<Option<String>>(1);
    let started = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.try_send(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
    let handle = Handle { stream, _output: output, _queues: queues, counters, stopped: false };
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(None) => {}
        Ok(Some(e)) => {
            let mut h = handle;
            h.stopped = true;
            return Err(anyhow!("startCapture: {e}")).context("ScreenCaptureKit (Screen Recording permission?)");
        }
        Err(_) => return Err(anyhow!("startCapture did not complete within 5 s")),
    }
    info!(
        "ScreenCaptureKit stream started: {source:?} -> {}x{} NV12 @ {fps} fps, queue depth {QUEUE_DEPTH}, audio {}",
        flags.canvas.0,
        flags.canvas.1,
        audio.is_some()
    );
    Ok(handle)
}
