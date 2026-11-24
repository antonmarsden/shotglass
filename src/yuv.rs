//! The export's YUV pass (#116): the renderer's final picture converted to 4:2:0
//! Y'CbCr on the GPU (`glsl/yuv.glsl`: BT.709, limited range, chroma sited left) and
//! read back through a ring of pixel pack buffers with fences, oldest first, so the
//! export never waits for the frame it just drew.
//!
//! A frame comes out as one I420 buffer of `w × h × 3/2` samples (u16 for 10-bit,
//! u8 for 8-bit, as little-endian bytes): Y rows, then rows of Cb followed by the
//! matching row of Cr. The encoder reads the planes in place with strides.

use crate::gfx::Renderer;
use crate::shader::Dialect;
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::collections::VecDeque;
use std::sync::Arc;
use toodee::TooDeeOps;

const YUV: &str = include_str!("glsl/yuv.glsl");
const VERTEX: &str = "void main(){\n  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n}\n";

/// The YUV pass's fragment shader, for the strict check.
pub fn fragment_source(dialect: Dialect) -> String {
    format!("{}{YUV}", dialect.version_line())
}

/// Bytes of one I420 frame.
pub fn frame_bytes(w: usize, h: usize, depth: u8) -> usize {
    w * h * 3 / 2 * if depth > 8 { 2 } else { 1 }
}

/// The same conversion on the CPU, in f64 (tests; the GPU's f32 result agrees on
/// the colours the selftest uses). `rgba` is top row first.
pub fn reference(rgba: &crate::gfx::Frame, depth: u8) -> Vec<u16> {
    let (w, h) = (rgba.num_cols(), rgba.num_rows());
    let scale = f64::from(1u32 << (depth - 8));
    let max = f64::from((1u32 << depth) - 1);
    let q = |v: f64| (v * scale + 0.5).floor().clamp(0.0, max) as u16;
    let px = |x: i64, y: i64| {
        let p = rgba[(
            x.clamp(0, w as i64 - 1) as usize,
            y.clamp(0, h as i64 - 1) as usize,
        )];
        [p[0], p[1], p[2]].map(|c| f64::from(c) / 255.0)
    };
    let luma = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        for x in 0..w {
            out.push(q(16.0 + 219.0 * luma(px(x as i64, y as i64))));
        }
    }
    for cy in 0..h / 2 {
        for cr in [false, true] {
            for cx in 0..w / 2 {
                let (x, y) = (2 * cx as i64, 2 * cy as i64);
                let mut c = [0.0; 3];
                for dy in 0..2 {
                    for (dx, wt) in [(-1, 1.0), (0, 2.0), (1, 1.0)] {
                        let p = px(x + dx, y + dy);
                        for i in 0..3 {
                            c[i] += wt * p[i];
                        }
                    }
                }
                let c = c.map(|v| v * 0.125);
                let l = luma(c);
                let d = if cr {
                    (c[0] - l) / 1.5748
                } else {
                    (c[2] - l) / 1.8556
                };
                out.push(q(128.0 + 224.0 * d));
            }
        }
    }
    out
}

/// A frame read back: its number and its I420 bytes.
pub struct YuvFrame {
    pub n: u64,
    pub data: Vec<u8>,
}

struct Slot {
    pbo: glow::Buffer,
    fence: glow::Fence,
    n: u64,
}

/// The conversion target, program and readback ring.
pub struct YuvReader {
    gl: Arc<glow::Context>,
    prog: glow::Program,
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    w: usize,
    h: usize,
    depth: u8,
    ring: usize,
    in_flight: VecDeque<Slot>,
    free: Vec<glow::Buffer>,
    /// Spare frame buffers handed back by the consumer.
    spare: Vec<Vec<u8>>,
    /// Times `collect(.., true)` found the oldest frame not yet finished and waited.
    pub waits: usize,
}

impl YuvReader {
    /// A reader for w × h pictures (both even) at `depth` bits (8 or 10), with up to
    /// `ring` frames in flight.
    pub fn new(r: &Renderer, w: usize, h: usize, depth: u8, ring: usize) -> Result<YuvReader> {
        if w % 2 == 1 || h % 2 == 1 {
            bail!("{w}x{h}: 4:2:0 needs an even width and height");
        }
        if depth != 8 && depth != 10 {
            bail!("bit depth {depth}: 8 or 10");
        }
        let gl = r.gl().clone();
        let dialect = r.dialect;
        unsafe {
            let prog = gl.create_program().map_err(|e| anyhow!(e))?;
            let mut shaders = vec![];
            for (kind, src) in [
                (
                    glow::VERTEX_SHADER,
                    format!("{}{VERTEX}", dialect.version_line()),
                ),
                (glow::FRAGMENT_SHADER, fragment_source(dialect)),
            ] {
                let s = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
                gl.shader_source(s, &src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    let log = gl.get_shader_info_log(s);
                    gl.delete_shader(s);
                    gl.delete_program(prog);
                    bail!("the YUV pass didn't compile:\n{log}");
                }
                gl.attach_shader(prog, s);
                shaders.push(s);
            }
            gl.link_program(prog);
            for s in shaders {
                gl.detach_shader(prog, s);
                gl.delete_shader(s);
            }
            if !gl.get_program_link_status(prog) {
                let log = gl.get_program_info_log(prog);
                gl.delete_program(prog);
                bail!("the YUV pass didn't link:\n{log}");
            }
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let (internal, format, ty) = Self::formats(depth);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                w as i32,
                (h + h / 2) as i32,
                0,
                format,
                ty,
                glow::PixelUnpackData::Slice(None),
            );
            for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::NEAREST as i32);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let ok = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            let reader = YuvReader {
                gl: gl.clone(),
                prog,
                vao,
                fbo,
                tex,
                w,
                h,
                depth,
                ring: ring.max(1),
                in_flight: VecDeque::new(),
                free: vec![],
                spare: vec![],
                waits: 0,
            };
            if !ok {
                bail!("the YUV target ({w}x{}) isn't renderable", h + h / 2);
            }
            Ok(reader)
        }
    }

    fn formats(depth: u8) -> (u32, u32, u32) {
        if depth > 8 {
            (glow::R16UI, glow::RED_INTEGER, glow::UNSIGNED_SHORT)
        } else {
            (glow::R8UI, glow::RED_INTEGER, glow::UNSIGNED_BYTE)
        }
    }

    pub fn frame_bytes(&self) -> usize {
        frame_bytes(self.w, self.h, self.depth)
    }

    /// Whether another frame can be submitted without collecting one first.
    pub fn has_room(&self) -> bool {
        self.in_flight.len() < self.ring
    }

    /// Hand a collected frame's buffer back for reuse.
    pub fn recycle(&mut self, data: Vec<u8>) {
        if self.spare.len() < self.ring + 2 {
            self.spare.push(data);
        }
    }

    /// Convert the renderer's current output (frame `n`) and start reading it back.
    /// Never waits for the GPU. Panics if the ring is full (`has_room`).
    pub fn submit(&mut self, r: &Renderer, src: glow::Texture, n: u64) -> Result<()> {
        assert!(self.has_room(), "YUV readback ring full");
        let gl = &self.gl;
        let (w, h) = (self.w as i32, self.h as i32);
        let rows = h + h / 2;
        let bytes = self.frame_bytes() as i32;
        let (_, format, ty) = Self::formats(self.depth);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.viewport(0, 0, w, rows);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.use_program(Some(self.prog));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src));
            gl.bind_sampler(0, None);
            let loc = |n: &str| gl.get_uniform_location(self.prog, n);
            gl.uniform_1_i32(loc("sg_src").as_ref(), 0);
            gl.uniform_2_i32(loc("sg_size").as_ref(), w, h);
            gl.uniform_1_i32(loc("sg_flipped").as_ref(), r.output_flipped() as i32);
            gl.uniform_1_f32(
                loc("sg_scale").as_ref(),
                f32::from(1u16 << (self.depth - 8)),
            );
            gl.uniform_1_f32(loc("sg_max").as_ref(), ((1u32 << self.depth) - 1) as f32);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_vertex_array(None);
            gl.use_program(None);

            let pbo = match self.free.pop() {
                Some(b) => b,
                None => {
                    let b = gl.create_buffer().map_err(|e| anyhow!(e))?;
                    gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(b));
                    gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, bytes, glow::STREAM_READ);
                    b
                }
            };
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            // Rows are tightly packed; restore the defaults after (as read_pixels_into).
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.read_pixels(
                0,
                0,
                w,
                rows,
                format,
                ty,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            let fence = gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(|e| anyhow!(e))?;
            // Make sure the fence reaches the GPU, so polling it can ever succeed.
            gl.flush();
            self.in_flight.push_back(Slot { pbo, fence, n });
        }
        Ok(())
    }

    /// The oldest frame in flight, if it's finished (or, with `block`, once it is).
    /// None when nothing is in flight, or without `block` when the oldest isn't ready.
    pub fn collect(&mut self, block: bool) -> Option<YuvFrame> {
        let slot = self.in_flight.front()?;
        let gl = &self.gl;
        unsafe {
            let ready = matches!(
                gl.client_wait_sync(slot.fence, 0, 0),
                glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED
            );
            if !ready {
                if !block {
                    return None;
                }
                self.waits += 1;
                loop {
                    let s = gl.client_wait_sync(
                        slot.fence,
                        glow::SYNC_FLUSH_COMMANDS_BIT,
                        1_000_000_000,
                    );
                    if s != glow::TIMEOUT_EXPIRED {
                        break;
                    }
                }
            }
        }
        let slot = self.in_flight.pop_front().unwrap();
        let bytes = self.frame_bytes();
        let mut data = self.spare.pop().unwrap_or_default();
        data.resize(bytes, 0);
        unsafe {
            gl.delete_sync(slot.fence);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(slot.pbo));
            let p =
                gl.map_buffer_range(glow::PIXEL_PACK_BUFFER, 0, bytes as i32, glow::MAP_READ_BIT);
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(p, data.as_mut_ptr(), bytes);
                gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            }
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
        self.free.push(slot.pbo);
        Some(YuvFrame { n: slot.n, data })
    }
}

impl Drop for YuvReader {
    fn drop(&mut self) {
        let gl = &self.gl;
        unsafe {
            for s in self.in_flight.drain(..) {
                gl.delete_sync(s.fence);
                gl.delete_buffer(s.pbo);
            }
            for b in self.free.drain(..) {
                gl.delete_buffer(b);
            }
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.tex);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.prog);
        }
    }
}

/// Little-endian I420 bytes as samples.
pub fn samples(data: &[u8], depth: u8) -> Vec<u16> {
    if depth > 8 {
        data.as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect()
    } else {
        data.iter().map(|&b| u16::from(b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_matches_bt709_limited_range() {
        // A flat 2x2 frame per colour: its Y, Cb, Cr at 10 and 8 bits.
        let cases: [([u8; 3], [u16; 3], [u16; 3]); 4] = [
            ([255, 255, 255], [940, 512, 512], [235, 128, 128]),
            ([0, 0, 0], [64, 512, 512], [16, 128, 128]),
            ([255, 0, 0], [250, 409, 960], [63, 102, 240]),
            ([0, 0, 255], [127, 960, 471], [32, 240, 118]),
        ];
        for (rgb, ten, eight) in cases {
            let f = crate::gfx::Frame::init(2, 2, [rgb[0], rgb[1], rgb[2], 255]);
            for (depth, want) in [(10, ten), (8, eight)] {
                let s = reference(&f, depth);
                assert_eq!([s[0], s[4], s[5]], want, "{rgb:?} at {depth} bits");
                assert!(s[..4].iter().all(|&v| v == want[0]));
            }
        }
        assert_eq!(frame_bytes(4, 2, 10), 24);
        assert_eq!(frame_bytes(4, 2, 8), 12);
    }

    #[test]
    fn reference_sites_chroma_left() {
        // Red in column 0 only: the first chroma sample sees it at weight 3/4 (the
        // edge column counts twice), the second (centred on column 2) doesn't.
        let mut f = crate::gfx::Frame::init(4, 2, [0, 0, 0, 255]);
        for y in 0..2 {
            f[(0, y)] = [255, 0, 0, 255];
        }
        let s = reference(&f, 8);
        let cr = &s[8 + 2..8 + 4];
        assert!(cr[0] > 200, "{cr:?}");
        assert_eq!(cr[1], 128);
    }

    #[test]
    fn yuv_pass_is_strictly_valid() {
        if !crate::validate::AVAILABLE {
            return;
        }
        for d in [Dialect::BASELINE, Dialect::WEB] {
            let src = crate::shader::Assembled {
                text: fragment_source(d),
                dialect: d,
                files: vec!["yuv.glsl".into()],
                warnings: vec![],
                clashes: vec![],
            };
            let res = crate::validate::validate(&src, crate::validate::Stage::Fragment);
            assert!(res.is_ok(), "{d}: {res:?}");
        }
    }
}
