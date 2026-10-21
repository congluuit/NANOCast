//! Windows sender pipeline, shared by the `nanocast` app and `spike-win`:
//! Windows Graphics Capture (monitor or one window, paced, unchanged frames skipped) ->
//! BGRA->NV12 (GPU video processor, texture straight into the encoder; CPU/rayon fallback) ->
//! Media Foundation H.264 hardware encoder (async MFT, first working one in merit order) ->
//! paced UDP (`nanocast_core::protocol::PacedSender`) + TCP control channel.
//!
//! `session::CastSession` wires everything together for the GUI; the modules are also
//! usable one by one (the spike measures each stage).

#[cfg(windows)]
pub mod audio;
pub mod convert;

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod control_link;
#[cfg(windows)]
pub mod gpu;
#[cfg(windows)]
pub mod mf_encoder;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod sources;
#[cfg(windows)]
pub mod speaker;
