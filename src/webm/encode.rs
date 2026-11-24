//! The encoders behind `export --format webm`: SVT-AV1 (through `svt_shim.c`) and
//! libopus (through the `opus` crate). Built only with the `webm` feature.

use anyhow::{Result, anyhow, bail};
use std::ffi::{CStr, CString, c_char, c_int};

#[repr(C)]
struct SgSvt {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn sg_svt_new(out: *mut *mut SgSvt) -> c_int;
    fn sg_svt_set(s: *mut SgSvt, name: *const c_char, value: *const c_char) -> c_int;
    fn sg_svt_init(s: *mut SgSvt) -> c_int;
    fn sg_svt_header(s: *mut SgSvt, buf: *mut u8, cap: u32) -> c_int;
    fn sg_svt_send(
        s: *mut SgSvt,
        y: *const u8,
        cb: *const u8,
        cr: *const u8,
        y_stride: u32,
        c_stride: u32,
        pts: i64,
        bytes: u32,
    ) -> c_int;
    fn sg_svt_send_eos(s: *mut SgSvt) -> c_int;
    fn sg_svt_receive(
        s: *mut SgSvt,
        done: c_int,
        data: *mut *const u8,
        len: *mut u32,
        pts: *mut i64,
        key: *mut c_int,
    ) -> c_int;
    fn sg_svt_version() -> *const c_char;
    fn sg_svt_free(s: *mut SgSvt);
}

/// SVT-AV1's version string.
pub fn svt_version() -> String {
    // SAFETY: SVT returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(sg_svt_version()) }
        .to_string_lossy()
        .into_owned()
}

/// libopus's version string.
pub fn opus_version() -> &'static str {
    opus::version()
}

/// How to spend bits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rate {
    /// Constant quality, SVT-AV1 CRF 0..=63 (lower is better).
    Quality(u8),
    /// Target bitrate in kbit/s (SVT-AV1 VBR).
    Bitrate(u32),
}

/// AV1 encoder settings.
#[derive(Clone, Debug)]
pub struct Av1Settings {
    pub width: usize,
    pub height: usize,
    pub fps: (u32, u32),
    /// 8 or 10.
    pub depth: u8,
    pub rate: Rate,
    /// SVT-AV1 preset 0..=13 (higher is faster).
    pub speed: u8,
    /// Frames between keyframes.
    pub keyint: u32,
    /// Film-grain denoising and synthesis strength (0 = off).
    pub grain: u8,
}

impl Av1Settings {
    /// SVT-AV1 parameters, by name.
    pub fn params(&self) -> Vec<(&'static str, String)> {
        let mut p = vec![
            ("width", self.width.to_string()),
            ("height", self.height.to_string()),
            ("fps-num", self.fps.0.to_string()),
            ("fps-denom", self.fps.1.to_string()),
            ("input-depth", self.depth.to_string()),
            ("preset", self.speed.to_string()),
            ("keyint", self.keyint.to_string()),
            // Closed GOPs: every keyframe is a seek point (a cluster and a cue).
            ("irefresh-type", "kf".into()),
            ("color-primaries", "bt709".into()),
            ("transfer-characteristics", "bt709".into()),
            ("matrix-coefficients", "bt709".into()),
            ("color-range", "studio".into()),
            ("chroma-sample-position", "left".into()),
        ];
        match self.rate {
            Rate::Quality(q) => p.push(("crf", q.to_string())),
            Rate::Bitrate(k) => {
                p.push(("rc", "vbr".into()));
                p.push(("tbr", k.to_string()));
            }
        }
        if self.grain > 0 {
            p.push(("film-grain", self.grain.to_string()));
            p.push(("film-grain-denoise", "1".into()));
        }
        p
    }
}

/// An encoded temporal unit.
pub struct Packet {
    pub data: Vec<u8>,
    /// The frame number it shows.
    pub pts: i64,
    pub keyframe: bool,
}

pub struct Av1Encoder {
    s: *mut SgSvt,
    w: usize,
    h: usize,
    depth: u8,
    header: Vec<u8>,
    sent_eos: bool,
}

// SAFETY: the encoder handle is only used through &mut self, from one thread at a time.
unsafe impl Send for Av1Encoder {}

impl Av1Encoder {
    pub fn new(set: &Av1Settings) -> Result<Av1Encoder> {
        let mut s = std::ptr::null_mut();
        // SAFETY: plain C calls on a handle we own; strings outlive the calls.
        unsafe {
            if sg_svt_new(&mut s) != 0 {
                bail!("SVT-AV1 couldn't create an encoder");
            }
            // Owns the handle from here on, so an early return frees it.
            let mut enc = Av1Encoder {
                s,
                w: set.width,
                h: set.height,
                depth: set.depth,
                header: vec![],
                sent_eos: false,
            };
            for (name, value) in set.params() {
                let (n, v) = (CString::new(name)?, CString::new(value.clone())?);
                if sg_svt_set(s, n.as_ptr(), v.as_ptr()) != 0 {
                    bail!("SVT-AV1 rejected {name} = {value}");
                }
            }
            if sg_svt_init(s) != 0 {
                bail!("SVT-AV1 couldn't start with these settings ({set:?})");
            }
            let mut buf = vec![0u8; 1024];
            let n = sg_svt_header(s, buf.as_mut_ptr(), buf.len() as u32);
            if n <= 0 {
                bail!("SVT-AV1 gave no sequence header");
            }
            buf.truncate(n as usize);
            enc.header = buf;
            Ok(enc)
        }
    }

    /// The sequence header OBU (for `av1C`).
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// Send one I420 frame (`yuv::YuvReader`'s layout: Y rows, then rows of Cb
    /// followed by Cr), little-endian u16 samples above 8 bits.
    pub fn send(&mut self, frame: &[u8], pts: i64) -> Result<()> {
        let (w, h) = (self.w, self.h);
        let bps = if self.depth > 8 { 2 } else { 1 };
        let bytes = crate::yuv::frame_bytes(w, h, self.depth);
        if frame.len() != bytes {
            bail!("frame of {} bytes, want {bytes}", frame.len());
        }
        let p = frame.as_ptr();
        // SAFETY: the planes lie inside `frame`, which outlives the call (SVT copies
        // the picture into its own buffers before returning).
        let r = unsafe {
            sg_svt_send(
                self.s,
                p,
                p.add(w * h * bps),
                p.add((w * h + w / 2) * bps),
                w as u32,
                w as u32,
                pts,
                bytes as u32,
            )
        };
        if r != 0 {
            bail!("SVT-AV1 refused frame {pts}");
        }
        Ok(())
    }

    /// The end of the input.
    pub fn finish(&mut self) -> Result<()> {
        if !self.sent_eos {
            self.sent_eos = true;
            // SAFETY: plain C call on our handle.
            if unsafe { sg_svt_send_eos(self.s) } != 0 {
                bail!("SVT-AV1 refused the end of the stream");
            }
        }
        Ok(())
    }

    /// The next packet: Ok(None) when none is ready yet (before `finish`) or the
    /// stream has ended (after it).
    pub fn receive(&mut self) -> Result<Option<Packet>> {
        let (mut data, mut len, mut pts, mut key) = (std::ptr::null(), 0u32, 0i64, 0);
        // SAFETY: the out-pointers are valid; the packet is copied before the next call.
        let r = unsafe {
            sg_svt_receive(
                self.s,
                self.sent_eos as c_int,
                &mut data,
                &mut len,
                &mut pts,
                &mut key,
            )
        };
        match r {
            0 => {
                let data = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
                Ok(Some(Packet {
                    data,
                    pts,
                    keyframe: key != 0,
                }))
            }
            1 | 2 => Ok(None),
            e => Err(anyhow!("SVT-AV1 failed to encode (error {e})")),
        }
    }
}

impl Drop for Av1Encoder {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and freed once.
        unsafe { sg_svt_free(self.s) };
    }
}

/// Opus at 48 kHz in 20 ms packets.
pub const OPUS_FRAME: usize = 960;

/// Opus encoding of interleaved stereo f32 at 48 kHz, pushed in pieces of any size.
/// The decoder drops `pre_skip` samples at the start and `padding` at the end (the
/// last packet's DiscardPadding), so what plays is exactly what was pushed.
pub struct OpusPackets {
    enc: opus::Encoder,
    pending: Vec<f32>,
    out: Vec<u8>,
    pub packets: Vec<Vec<u8>>,
    pub pre_skip: u16,
    /// Sample frames pushed.
    pub samples: usize,
    /// Samples of silence in the last packet beyond the input (set by `finish`).
    pub padding: usize,
}

impl OpusPackets {
    pub fn new(kbps: u32) -> Result<OpusPackets> {
        let e = |e: opus::Error| anyhow!("libopus: {e}");
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(e)?;
        enc.set_bitrate(opus::Bitrate::Bits(kbps as i32 * 1000))
            .map_err(e)?;
        let pre_skip = enc.get_lookahead().map_err(e)? as u16;
        Ok(OpusPackets {
            enc,
            pending: Vec::with_capacity(OPUS_FRAME * 4),
            out: vec![0; 4000],
            packets: vec![],
            pre_skip,
            samples: 0,
            padding: 0,
        })
    }

    fn encode_pending(&mut self) -> Result<()> {
        let mut start = 0;
        while self.pending.len() - start >= OPUS_FRAME * 2 {
            let n = self
                .enc
                .encode_float(&self.pending[start..start + OPUS_FRAME * 2], &mut self.out)
                .map_err(|e| anyhow!("libopus: {e}"))?;
            self.packets.push(self.out[..n].to_vec());
            start += OPUS_FRAME * 2;
        }
        self.pending.drain(..start);
        Ok(())
    }

    /// Interleaved stereo samples.
    pub fn push(&mut self, stereo: &[f32]) -> Result<()> {
        self.samples += stereo.len() / 2;
        self.pending.extend_from_slice(stereo);
        self.encode_pending()
    }

    /// Flush the encoder's lookahead with silence and pad the last packet.
    pub fn finish(&mut self) -> Result<()> {
        let total = self.samples + self.pre_skip as usize;
        let count = total.div_ceil(OPUS_FRAME);
        self.padding = count * OPUS_FRAME - total;
        let fed = self.packets.len() * OPUS_FRAME + self.pending.len() / 2;
        self.pending
            .resize(self.pending.len() + (count * OPUS_FRAME - fed) * 2, 0.0);
        self.encode_pending()?;
        debug_assert_eq!(self.packets.len(), count);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webm::av1;

    #[test]
    fn opus_drops_exactly_its_padding() {
        let n = 48_000 + 123;
        let mut o = OpusPackets::new(160).unwrap();
        let mut left = n;
        while left > 0 {
            let k = left.min(777);
            o.push(&vec![0.1f32; k * 2]).unwrap();
            left -= k;
        }
        o.finish().unwrap();
        assert_eq!(o.samples, n);
        assert_eq!(
            o.packets.len() * OPUS_FRAME,
            n + o.pre_skip as usize + o.padding
        );
        assert!(o.padding < OPUS_FRAME);
        assert!(o.pre_skip > 0);
    }

    #[test]
    fn svt_encodes_a_small_10_bit_stream() {
        let (w, h) = (64usize, 48usize);
        let set = Av1Settings {
            width: w,
            height: h,
            fps: (24, 1),
            depth: 10,
            rate: Rate::Quality(40),
            speed: 12,
            keyint: 8,
            grain: 0,
        };
        let mut enc = Av1Encoder::new(&set).unwrap();
        let (seq, _) = av1::find_sequence_header(enc.header()).unwrap();
        assert_eq!(seq.bit_depth, 10);
        assert_eq!(seq.color, Some((1, 1, 1)), "BT.709 tagged");
        assert!(!seq.full_range);
        assert_eq!(seq.chroma_sample_position, 1, "left (vertical)");
        let frames = 20;
        let mut got = vec![];
        for n in 0..frames {
            let mut f = vec![0u8; crate::yuv::frame_bytes(w, h, 10)];
            for (i, s) in f.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                *s = (((i + n * 7) % 700 + 64) as u16).to_le_bytes();
            }
            enc.send(&f, n as i64).unwrap();
            while let Some(p) = enc.receive().unwrap() {
                got.push((p.pts, p.keyframe));
            }
        }
        enc.finish().unwrap();
        while let Some(p) = enc.receive().unwrap() {
            got.push((p.pts, p.keyframe));
        }
        let pts: Vec<i64> = got.iter().map(|g| g.0).collect();
        assert_eq!(
            pts,
            (0..frames as i64).collect::<Vec<_>>(),
            "one packet per frame, in order"
        );
        let keys: Vec<i64> = got.iter().filter(|g| g.1).map(|g| g.0).collect();
        assert_eq!(keys, [0, 8, 16]);
    }
}
