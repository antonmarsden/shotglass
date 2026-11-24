//! Built-in WebM export (#116): AV1 + Opus in shotglass's own WebM writer.
//!
//! `mux` and `av1` are plain Rust and always built; the encoders need the `webm`
//! cargo feature (SVT-AV1 and libopus).

#![allow(dead_code)] // until the export uses it (later commits of #116)

pub mod av1;
pub mod mux;
#[cfg(feature = "webm")]
pub mod encode;
