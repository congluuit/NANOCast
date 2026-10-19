//! NANOCast Linux sender pipeline (Phase 3). Compiles to nothing on other platforms.

#[cfg(target_os = "linux")]
pub mod platform {
    // PipeWire capture + VA-API encode go here.
}
