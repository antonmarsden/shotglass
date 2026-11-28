//! Decoding video frames (#73): the `VideoDecoder` trait (AV1 temporal units in,
//! Y'CbCr planes out), its software implementation (rav1d, the Rust port of dav1d),
//! and `Reader`, which serves "frame k" from a clip by decoding on from where it is
//! or from the keyframe at or before k.
//!
//! Hardware decoders (#161) would be further `VideoDecoder`s; AV1 decoding is
//! bit-exact by specification, so they must give the same planes.

use super::Planes;
use crate::webm::demux::Clip;
use anyhow::{Result, anyhow, bail};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

/// An AV1 decoder: temporal units in, decoded frames out, in presentation order.
pub trait VideoDecoder: Send {
    /// Forget everything: the next unit sent is a keyframe.
    fn reset(&mut self);
    /// One temporal unit (a WebM block).
    fn send(&mut self, unit: &[u8]) -> Result<()>;
    /// The next decoded frame, if one is ready.
    fn receive(&mut self) -> Result<Option<Planes>>;
}

/// Whether this build can decode video.
pub const AVAILABLE: bool = cfg!(rav1d);

/// Why a build can't decode video, for errors.
pub const UNAVAILABLE: &str = "this build of shotglass has no video decoder (it was built \
     without the `video` feature, or for 32-bit ARM or RISC-V, where rav1d needs a nightly compiler)";

/// A decoder for this clip's codec.
pub fn decoder_for(clip: &Clip) -> Result<Box<dyn VideoDecoder>> {
    if clip.track.codec != "V_AV1" {
        bail!(
            "{} video; shotglass decodes AV1 only",
            codec_name(&clip.track.codec)
        );
    }
    #[cfg(rav1d)]
    {
        Ok(Box::new(dav1d::Dav1d::new()?))
    }
    #[cfg(not(rav1d))]
    {
        bail!("{UNAVAILABLE}")
    }
}

/// `V_VP9` → `VP9`.
pub fn codec_name(id: &str) -> &str {
    id.strip_prefix("V_").unwrap_or(id)
}

/// Serves frames of one clip: one file handle, one decoder and its position.
pub struct Reader {
    clip: Arc<Clip>,
    file: BufReader<File>,
    dec: Box<dyn VideoDecoder>,
    /// The next frame to send.
    next_in: usize,
    /// The frame number the next picture out will be.
    next_out: usize,
    /// Whether the decoder has been started at a keyframe.
    started: bool,
    /// Seeks so far (tests).
    pub seeks: u64,
}

impl Reader {
    pub fn open(path: &Path, clip: Arc<Clip>) -> Result<Reader> {
        let dec = decoder_for(&clip)?;
        Ok(Reader {
            file: BufReader::new(File::open(path)?),
            clip,
            dec,
            next_in: 0,
            next_out: 0,
            started: false,
            seeks: 0,
        })
    }

    /// The frame the next `frame` call can serve without seeking, if any.
    pub fn position(&self) -> Option<usize> {
        self.started.then_some(self.next_out)
    }

    /// Whether frame `k` is reached by decoding on (no seek).
    pub fn reaches(&self, k: usize) -> bool {
        self.started && k >= self.next_out && self.clip.keyframe_at_or_before(k) <= self.next_out
    }

    /// Frame `k`. Frames decoded on the way are passed to `skipped` (frame number,
    /// planes), so a caller can keep them.
    pub fn frame_with(
        &mut self,
        k: usize,
        mut skipped: impl FnMut(usize, Planes),
    ) -> Result<Planes> {
        let n = self.clip.frames.len();
        if k >= n {
            bail!("frame {k} is past the end ({n} frames)");
        }
        if !self.reaches(k) {
            let key = self.clip.keyframe_at_or_before(k);
            self.dec.reset();
            self.next_in = key;
            self.next_out = key;
            self.started = true;
            self.seeks += 1;
        }
        let mut idle = 0;
        loop {
            while let Some(p) = self.dec.receive()? {
                let j = self.next_out;
                self.next_out += 1;
                idle = 0;
                if j == k {
                    return Ok(p);
                }
                skipped(j, p);
            }
            if self.next_in < n {
                let unit = self.clip.read_frame(&mut self.file, self.next_in)?;
                self.dec.send(&unit).map_err(|e| {
                    self.started = false;
                    anyhow!("frame {}: {e}", self.next_in)
                })?;
                self.next_in += 1;
            } else {
                idle += 1;
                if idle > 64 {
                    self.started = false;
                    bail!("the decoder gave no picture for frame {}", self.next_out);
                }
            }
        }
    }

    pub fn frame(&mut self, k: usize) -> Result<Planes> {
        self.frame_with(k, |_, _| {})
    }
}

#[cfg(rav1d)]
mod dav1d {
    //! rav1d through its dav1d-compatible API.

    use super::super::Planes;
    use super::VideoDecoder;
    use anyhow::{Result, bail};
    use rav1d::include::dav1d::data::Dav1dData;
    use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
    use rav1d::include::dav1d::headers::{
        DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
    };
    use rav1d::include::dav1d::picture::Dav1dPicture;
    use rav1d::src::lib::{
        dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
        dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
    };
    use std::mem::MaybeUninit;
    use std::ptr::NonNull;

    const EAGAIN: i32 = -libc::EAGAIN;

    pub struct Dav1d {
        ctx: Option<Dav1dContext>,
        /// Data `send` couldn't hand over yet (the decoder wanted pictures out first).
        pending: Dav1dData,
    }

    // SAFETY: a dav1d context may be used from any one thread at a time; `Dav1d` is
    // only ever used through `&mut`.
    unsafe impl Send for Dav1d {}

    impl Dav1d {
        pub fn new() -> Result<Dav1d> {
            let mut s = MaybeUninit::<Dav1dSettings>::uninit();
            // SAFETY: `dav1d_default_settings` writes a whole `Dav1dSettings`.
            let mut s = unsafe {
                dav1d_default_settings(NonNull::new(s.as_mut_ptr()).unwrap());
                s.assume_init()
            };
            // Several readers run at once on the decode pool: a few threads each, and
            // one frame in flight, so a sent unit comes out before the next is sent.
            s.n_threads = 2;
            s.max_frame_delay = 1;
            let mut ctx: Option<Dav1dContext> = None;
            // SAFETY: valid pointers to a settings struct and an output slot.
            let r = unsafe { dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut s)) };
            if r.0 != 0 || ctx.is_none() {
                bail!("the AV1 decoder didn't start (error {})", r.0);
            }
            Ok(Dav1d {
                ctx,
                pending: Dav1dData::default(),
            })
        }

        fn push_pending(&mut self) -> Result<bool> {
            if self.pending.sz == 0 {
                return Ok(true);
            }
            // SAFETY: `pending` is a live Dav1dData from `dav1d_data_create`.
            let r = unsafe { dav1d_send_data(self.ctx, NonNull::new(&mut self.pending)) };
            match r.0 {
                0 => Ok(self.pending.sz == 0),
                EAGAIN => Ok(false),
                e => {
                    // SAFETY: as above; unref frees it and zeroes the struct.
                    unsafe { dav1d_data_unref(NonNull::new(&mut self.pending)) };
                    bail!("AV1 decode error {e}")
                }
            }
        }
    }

    impl Drop for Dav1d {
        fn drop(&mut self) {
            // SAFETY: frees pending data (a no-op when empty) and the context.
            unsafe {
                dav1d_data_unref(NonNull::new(&mut self.pending));
                dav1d_close(NonNull::new(&mut self.ctx));
            }
        }
    }

    impl VideoDecoder for Dav1d {
        fn reset(&mut self) {
            // SAFETY: as in Drop; flush drops all decoder state.
            unsafe {
                dav1d_data_unref(NonNull::new(&mut self.pending));
                dav1d_flush(self.ctx.unwrap());
            }
        }

        fn send(&mut self, unit: &[u8]) -> Result<()> {
            if self.pending.sz != 0 {
                bail!("a unit was sent before the previous one was taken");
            }
            // SAFETY: allocates `unit.len()` bytes owned by `pending`, then fills them.
            unsafe {
                let p = dav1d_data_create(NonNull::new(&mut self.pending), unit.len());
                if p.is_null() {
                    bail!("out of memory for a {}-byte frame", unit.len());
                }
                std::ptr::copy_nonoverlapping(unit.as_ptr(), p, unit.len());
            }
            self.push_pending()?;
            Ok(())
        }

        fn receive(&mut self) -> Result<Option<Planes>> {
            let mut pic = Dav1dPicture::default();
            // SAFETY: `pic` is a default picture for dav1d to fill.
            let r = unsafe { dav1d_get_picture(self.ctx, NonNull::new(&mut pic)) };
            let out = match r.0 {
                0 => {
                    let planes = copy(&pic);
                    // SAFETY: releases the picture dav1d gave us.
                    unsafe { dav1d_picture_unref(NonNull::new(&mut pic)) };
                    Some(planes?)
                }
                EAGAIN => None,
                e => bail!("AV1 decode error {e}"),
            };
            // A unit held back for want of room goes in now.
            if self.pending.sz != 0 {
                self.push_pending()?;
            }
            Ok(out)
        }
    }

    fn copy(pic: &Dav1dPicture) -> Result<Planes> {
        let (w, h) = (pic.p.w as usize, pic.p.h as usize);
        let bits = pic.p.bpc as u8;
        let sub = match pic.p.layout {
            DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
            DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
            DAV1D_PIXEL_LAYOUT_I400 => (1, 1),
            _ => (0, 0),
        };
        let mono = pic.p.layout == DAV1D_PIXEL_LAYOUT_I400;
        let wide = bits > 8;
        let plane = |i: usize, pw: usize, ph: usize| -> Vec<u16> {
            let stride = pic.stride[i.min(1)] as usize;
            let base = pic.data[i].unwrap().as_ptr() as *const u8;
            let mut v = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                // SAFETY: dav1d's planes are `ph` rows of `stride` bytes, each holding
                // `pw` samples of 1 (8-bit) or 2 (high bit depth) bytes.
                unsafe {
                    let row = base.add(y * stride);
                    if wide {
                        let r = std::slice::from_raw_parts(row as *const u16, pw);
                        v.extend_from_slice(r);
                    } else {
                        let r = std::slice::from_raw_parts(row, pw);
                        v.extend(r.iter().map(|&b| u16::from(b)));
                    }
                }
            }
            v
        };
        if pic.data[0].is_none() {
            bail!("the decoder returned an empty picture");
        }
        let (cw, ch) = (w.div_ceil(1 << sub.0), h.div_ceil(1 << sub.1));
        let y = plane(0, w, h);
        let (u, v) = if mono {
            (vec![], vec![])
        } else {
            (plane(1, cw, ch), plane(2, cw, ch))
        };
        Ok(Planes {
            width: w,
            height: h,
            bits,
            sub,
            y,
            u,
            v,
        })
    }
}

#[cfg(all(test, rav1d, feature = "webm"))]
mod tests {
    use super::*;
    use crate::webm::demux;
    use crate::webm::encode::{Av1Settings, Rate};

    /// An I420 frame whose luma is flat at a level that names the frame, with a
    /// gradient in chroma.
    fn frame(w: usize, h: usize, depth: u8, n: usize) -> Vec<u8> {
        let level = |n: usize| 32 + (n * 9) % 180;
        let s = 1u16 << (depth - 8);
        let mut v: Vec<u16> = vec![level(n) as u16 * s; w * h];
        for cy in 0..h / 2 {
            v.extend((0..w / 2).map(|x| (100 + x as u16) * s)); // Cb row
            v.extend((0..w / 2).map(|_| (140 + cy as u16) * s)); // Cr row
        }
        if depth > 8 {
            v.iter().flat_map(|x| x.to_le_bytes()).collect()
        } else {
            v.iter().map(|&x| x as u8).collect()
        }
    }

    fn clip(dir: &Path, depth: u8, keyint: u32, n: usize) -> (std::path::PathBuf, Arc<Clip>) {
        let (w, h) = (64, 36);
        let path = dir.join(format!("c{depth}-{keyint}.webm"));
        let set = Av1Settings {
            width: w,
            height: h,
            fps: (30000, 1001),
            depth,
            rate: Rate::Quality(20),
            speed: 12,
            keyint,
            grain: 0,
            colour: crate::webm::ColourTags::BT709,
        };
        crate::webm::export::write_clip(&path, &set, (0..n).map(|k| frame(w, h, depth, k)))
            .unwrap();
        let c = Arc::new(demux::open(&path).unwrap());
        (path, c)
    }

    #[test]
    fn seek_equals_sequential_and_frames_are_in_order() {
        let dir = tempfile::tempdir().unwrap();
        for (depth, keyint) in [(8, 8), (10, 30)] {
            let n = 40;
            let (path, c) = clip(dir.path(), depth, keyint, n);
            assert_eq!(c.fps, (30000, 1001));
            assert_eq!(c.frames.len(), n);
            let mut seq = Reader::open(&path, c.clone()).unwrap();
            let all: Vec<Planes> = (0..n).map(|k| seq.frame(k).unwrap()).collect();
            assert_eq!(seq.seeks, 1, "a straight read seeks once");
            for (k, p) in all.iter().enumerate() {
                assert_eq!((p.width, p.height, p.bits), (64, 36, depth));
                let want = ((32 + (k * 9) % 180) << (depth - 8)) as i32;
                let got = i32::from(p.y[64 * 18 + 32]);
                assert!(
                    (got - want).abs() <= 2 << (depth - 8),
                    "frame {k}: luma {got}, want {want}"
                );
            }
            // Random order, backwards, across GOPs: every frame bit for bit.
            let mut r = Reader::open(&path, c.clone()).unwrap();
            for &k in &[39, 3, 17, 16, 0, 38, 9, 8, 25, 24, 23] {
                assert_eq!(r.frame(k).unwrap(), all[k], "depth {depth}, frame {k}");
            }
            // Frames passed on the way are the same frames.
            let mut r = Reader::open(&path, c.clone()).unwrap();
            let mut seen = vec![];
            let last = r.frame_with(n - 1, |j, p| seen.push((j, p))).unwrap();
            assert_eq!(last, all[n - 1]);
            let start = c.keyframe_at_or_before(n - 1);
            assert_eq!(seen.len(), n - 1 - start);
            for (j, p) in seen {
                assert_eq!(p, all[j]);
            }
        }
    }

    #[test]
    fn colour_tags_from_our_export() {
        let dir = tempfile::tempdir().unwrap();
        let (_, c) = clip(dir.path(), 8, 8, 2);
        let (seq, _) =
            crate::webm::av1::find_sequence_header_in_av1c(&c.track.codec_private).unwrap();
        let conv = super::super::Conversion::from_tags(&c.track.colour, Some(&seq), 36);
        assert_eq!(conv.matrix, super::super::Matrix::Bt709);
        assert!(!conv.full_range);
        assert_eq!(
            conv.siting,
            (super::super::Siting::CoSited, super::super::Siting::Half)
        );
    }
}
