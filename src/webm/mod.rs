//! Built-in WebM export (#116): AV1 + Opus in shotglass's own WebM writer.
//!
//! `mux` and `av1` are plain Rust and always built; the encoders need the `webm`
//! cargo feature (SVT-AV1 and libopus).

// Without the encoders, only the tests use the writer.
#![cfg_attr(not(feature = "webm"), allow(dead_code))]

pub mod av1;
pub mod demux;
#[cfg(feature = "webm")]
pub mod encode;
#[cfg(feature = "webm")]
pub mod export;
pub mod mux;

/// The codecs linked into this build for `export --format webm`, with their licences
/// (`shotglass licenses` lists them; release binaries carry their notices, #28).
#[cfg(feature = "webm")]
pub const ENCODER_LICENCES: &[(&str, &str)] = &[
    (
        "SVT-AV1",
        "BSD-3-Clause-Clear (parts BSD-2-Clause), with the AOMedia Patent License 1.0",
    ),
    ("libopus", "BSD-3-Clause"),
];
#[cfg(not(feature = "webm"))]
pub const ENCODER_LICENCES: &[(&str, &str)] = &[];
