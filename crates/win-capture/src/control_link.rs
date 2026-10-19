//! Sender side of the control channel; lives in `nanocast_core::control_link` (shared with
//! the other sender platforms) and is re-exported here so callers keep using this path.

pub use nanocast_core::control_link::*;
