//! Software fallback: Cisco OpenH264 built from source by the `openh264` crate (BSD, no
//! FFmpeg needed). Screen-content real-time mode, bitrate rate control without frame
//! skipping, low complexity, IDR on request; SPS/PPS come with every IDR.
//! Synchronous: each `submit` produces its packet right away.

use std::time::Instant;

use anyhow::{Context as _, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};

use crate::convert::nv12_to_i420;
use crate::encoder::{EncoderConfig, RawPacket};

pub(crate) struct OhEncoder {
    enc: Encoder,
    width: usize,
    height: usize,
    i420: Vec<u8>,
    out: Vec<RawPacket>,
}

impl OhEncoder {
    pub(crate) fn open(cfg: &EncoderConfig) -> Result<Self> {
        let config = OhConfig::new()
            .bitrate(BitRate::from_bps(cfg.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(cfg.fps.max(1) as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .complexity(Complexity::Low)
            .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.gop_frames.max(1)));
        let enc = Encoder::with_api_config(OpenH264API::from_source(), config).context("OpenH264 encoder")?;
        Ok(Self { enc, width: cfg.width as usize, height: cfg.height as usize, i420: Vec::new(), out: Vec::new() })
    }

    pub(crate) fn submit(&mut self, nv12: &[u8], pts_us: i64, captured: Instant, force_keyframe: bool) -> Result<()> {
        let submitted = Instant::now();
        let (w, h) = (self.width, self.height);
        nv12_to_i420(nv12, w as u32, h as u32, &mut self.i420);
        let (y, rest) = self.i420.split_at(w * h);
        let (u, v) = rest.split_at(w * h / 4);
        let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        if force_keyframe {
            self.enc.force_intra_frame();
        }
        let bs = self.enc.encode_at(&src, Timestamp::from_millis(pts_us.max(0) as u64 / 1000)).context("OpenH264 encode")?;
        if matches!(bs.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(());
        }
        let data = bs.to_vec();
        if !data.is_empty() {
            self.out.push(RawPacket { data, captured, submitted });
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self, out: &mut Vec<RawPacket>) {
        out.append(&mut self.out);
    }
}
