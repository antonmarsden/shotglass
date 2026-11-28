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

/// Colour description written into the AV1 sequence header and Matroska's Colour
/// element (ISO/IEC 23091-4 code points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourTags {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColourTags {
    /// BT.709, limited range: what export writes.
    pub const BT709: ColourTags = ColourTags {
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
    };

    /// SVT-AV1's names for these code points: (primaries, transfer, matrix); None
    /// when one isn't a value SVT-AV1 names.
    pub fn svt_names(&self) -> Option<(&'static str, &'static str, &'static str)> {
        let p = match self.primaries {
            1 => "bt709",
            4 => "bt470m",
            5 => "bt470bg",
            6 => "bt601",
            7 => "smpte240",
            9 => "bt2020",
            _ => return None,
        };
        let t = match self.transfer {
            1 => "bt709",
            4 => "bt470m",
            5 => "bt470bg",
            6 => "bt601",
            7 => "smpte240",
            13 => "srgb",
            14 => "bt2020-10",
            15 => "bt2020-12",
            _ => return None,
        };
        let m = match self.matrix {
            1 => "bt709",
            5 => "bt470bg",
            6 => "bt601",
            7 => "smpte240",
            9 => "bt2020-ncl",
            _ => return None,
        };
        Some((p, t, m))
    }
}

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
