//! AAC-LC encoder: the Media Foundation AAC encoder MFT that ships with Windows
//! (CLSID_AACMFTEncoder, synchronous). PCM 16-bit 48 kHz stereo in, raw AAC frames
//! (1024 samples each, no ADTS) out. COM / Media Foundation must be initialised on the
//! calling thread (`nanocast_win::mf_encoder::MfRuntime`).

use std::mem::ManuallyDrop;

use anyhow::{Context as _, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const FRAME_SAMPLES: usize = 1024;
/// Allowed: 12000, 16000, 20000, 24000 bytes/s (96..192 kbps).
const BYTES_PER_SECOND: u32 = 20_000;
pub const BITRATE: u32 = BYTES_PER_SECOND * 8;

pub struct AacEncoder {
    transform: IMFTransform,
    out_size: u32,
}

impl AacEncoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER).context("AAC encoder MFT")?;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, BYTES_PER_SECOND)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            transform.SetOutputType(0, &out, 0).context("AAC SetOutputType")?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS as u32 * 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * CHANNELS as u32 * 2)?;
            transform.SetInputType(0, &inp, 0).context("AAC SetInputType")?;

            let info = transform.GetOutputStreamInfo(0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { transform, out_size: info.cbSize.max(8192) })
        }
    }

    /// Encodes interleaved stereo f32 PCM starting at sample `pos` (48 kHz); every finished
    /// AAC frame goes to `on_frame`.
    pub fn encode(&mut self, pcm: &[f32], pos: u64, mut on_frame: impl FnMut(Vec<u8>)) -> Result<()> {
        let bytes: Vec<u8> = pcm
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
            .collect();
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((pos * 10_000_000 / SAMPLE_RATE as u64) as i64)?;
            sample.SetSampleDuration((pcm.len() as u64 / CHANNELS as u64 * 10_000_000 / SAMPLE_RATE as u64) as i64)?;
            self.transform.ProcessInput(0, &sample, 0).context("AAC ProcessInput")?;
            loop {
                let buffer = MFCreateMemoryBuffer(self.out_size)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                let mut out = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let r = self.transform.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
                let sample = ManuallyDrop::take(&mut out.pSample);
                ManuallyDrop::drop(&mut out.pEvents);
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e).context("AAC ProcessOutput"),
                }
                let Some(sample) = sample else { return Ok(()) };
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut p, None, Some(&mut len))?;
                let frame = std::slice::from_raw_parts(p, len as usize).to_vec();
                buf.Unlock()?;
                if !frame.is_empty() {
                    on_frame(frame);
                }
            }
        }
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}
