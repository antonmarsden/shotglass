//! The GPU side of `shotglass quality`: float targets, the metric pass, the 4×4
//! reduction and the histogram. Only the reduction's last texels and the 256 histogram
//! counts come back for the numbers (heatmaps are read back for their PNGs).

use super::config::BINS;
use crate::gfx::FrameInfo;
use crate::shader::{Assembled, Dialect};
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use toodee::{TooDee, TooDeeOps};

/// Float RGBA texels as GL lays them out: bottom row first.
pub type Texels = TooDee<[f32; 4]>;

/// One reduction pass: each output texel combines a 4×4 block. The first pass reads
/// the measure (`.a` of the metric map); later ones read the previous level's two
/// targets: (sum, max, finite, non-finite) and (over, min, -, -). Non-finite is
/// tested on the bits ([`NONFINITE`]).
const REDUCE: &str = "
uniform sampler2D sg_src;
uniform sampler2D sg_src2;
uniform ivec2 sg_size;
uniform int sg_first;
uniform float sg_threshold;
layout(location = 0) out vec4 o0;
layout(location = 1) out vec4 o1;
void main() {
  ivec2 base = ivec2(gl_FragCoord.xy) * 4;
  float sum = 0.0, mx = -3.0e38, n = 0.0, bad = 0.0, over = 0.0, mn = 3.0e38;
  for (int y = 0; y < 4; y++) {
    for (int x = 0; x < 4; x++) {
      ivec2 p = base + ivec2(x, y);
      if (p.x >= sg_size.x || p.y >= sg_size.y) continue;
      vec4 v = texelFetch(sg_src, p, 0);
      if (sg_first == 1) {
        if (sg_nonfinite(v.a)) {
          bad += 1.0;
        } else {
          sum += v.a;
          mx = max(mx, v.a);
          mn = min(mn, v.a);
          n += 1.0;
          if (v.a > sg_threshold) over += 1.0;
        }
      } else {
        vec4 w = texelFetch(sg_src2, p, 0);
        sum += v.r;
        mx = max(mx, v.g);
        n += v.b;
        bad += v.a;
        over += w.r;
        mn = min(mn, w.g);
      }
    }
  }
  o0 = vec4(sum, mx, n, bad);
  o1 = vec4(over, mn, 0.0, 0.0);
}
";

/// NaN or Inf: the exponent bits are all ones. A bit test no optimiser can fold away,
/// unlike `isnan`/`isinf` or `x != x`; both passes use it, so they agree.
const NONFINITE: &str = "
bool sg_nonfinite(float x) { return (floatBitsToUint(x) & 0x7f800000u) == 0x7f800000u; }
";

/// The histogram: one point per pixel of the metric map, added (blending ONE, ONE)
/// into the bin its finite measure falls in; non-finite ones are drawn off-target.
const HISTOGRAM_VS: &str = "
uniform sampler2D sg_src;
uniform int sg_width;
uniform vec2 sg_range;
void main() {
  ivec2 p = ivec2(gl_VertexID % sg_width, gl_VertexID / sg_width);
  float v = texelFetch(sg_src, p, 0).a;
  gl_PointSize = 1.0;
  if (sg_nonfinite(v)) {
    gl_Position = vec4(2.0, 2.0, 0.0, 1.0);
    return;
  }
  float b = clamp(floor((v - sg_range.x) / (sg_range.y - sg_range.x) * BINS), 0.0, BINS - 1.0);
  gl_Position = vec4((b + 0.5) / BINS * 2.0 - 1.0, 0.0, 0.0, 1.0);
}
";

const HISTOGRAM_FS: &str = "
out vec4 o;
void main() { o = vec4(1.0); }
";

/// An RGBA32F render target with one or more colour attachments.
pub struct Target {
    pub tex: Vec<glow::Texture>,
    pub fbo: glow::Framebuffer,
    pub size: (i32, i32),
}

unsafe fn nearest(gl: &glow::Context) {
    unsafe {
        for (p, v) in [
            (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
            (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
    }
}

impl Target {
    pub unsafe fn new(gl: &glow::Context, w: i32, h: i32, attachments: u32) -> Result<Target> {
        unsafe {
            let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            let mut tex = vec![];
            for i in 0..attachments {
                let t = gl.create_texture().map_err(|e| anyhow!(e))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA32F as i32,
                    w,
                    h,
                    0,
                    glow::RGBA,
                    glow::FLOAT,
                    glow::PixelUnpackData::Slice(None),
                );
                nearest(gl);
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0 + i,
                    glow::TEXTURE_2D,
                    Some(t),
                    0,
                );
                tex.push(t);
            }
            let bufs: Vec<u32> = (0..attachments)
                .map(|i| glow::COLOR_ATTACHMENT0 + i)
                .collect();
            gl.draw_buffers(&bufs);
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            let t = Target {
                tex,
                fbo,
                size: (w, h),
            };
            if status != glow::FRAMEBUFFER_COMPLETE {
                t.delete(gl);
                bail!(
                    "this driver can't render to RGBA32F targets (framebuffer status {status:#x})"
                );
            }
            Ok(t)
        }
    }

    pub unsafe fn delete(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_framebuffer(self.fbo);
            for t in &self.tex {
                gl.delete_texture(*t);
            }
        }
    }

    /// Attachment `i`'s texels.
    pub unsafe fn read(&self, gl: &glow::Context, i: u32) -> Texels {
        let (w, h) = self.size;
        let mut px = Texels::init(w as usize, h as usize, [0.0; 4]);
        unsafe {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.read_buffer(glow::COLOR_ATTACHMENT0 + i);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
            let bytes: &mut [u8] = bytemuck_bytes(px.data_mut());
            gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::FLOAT,
                glow::PixelPackData::Slice(Some(bytes)),
            );
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        px
    }
}

/// Texels as the bytes GL reads into and uploads from.
fn bytemuck_bytes(px: &mut [[f32; 4]]) -> &mut [u8] {
    let len = std::mem::size_of_val(px);
    // SAFETY: [f32; 4] has no padding and every bit pattern is a valid f32.
    unsafe { std::slice::from_raw_parts_mut(px.as_mut_ptr().cast::<u8>(), len) }
}

/// Upload texels as an RGBA32F texture.
pub unsafe fn upload(gl: &glow::Context, px: &Texels) -> Result<glow::Texture> {
    let mut data = px.data().to_vec();
    unsafe {
        let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA32F as i32,
            px.num_cols() as i32,
            px.num_rows() as i32,
            0,
            glow::RGBA,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(Some(bytemuck_bytes(&mut data))),
        );
        nearest(gl);
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(tex)
    }
}

/// Everything the metric passes draw with, sized for w×h maps.
pub struct Gpu {
    vao: glow::VertexArray,
    pub map: Target,
    levels: Vec<Target>,
    hist: Target,
    reduce: glow::Program,
    histogram: glow::Program,
}

impl Gpu {
    pub unsafe fn new(gl: &glow::Context, d: Dialect, w: i32, h: i32) -> Result<Gpu> {
        unsafe {
            let v = d.version_line();
            let reduce = crate::shader::compile(
                gl,
                &Assembled {
                    text: format!("{v}{NONFINITE}{REDUCE}"),
                    dialect: d,
                    files: vec!["<shotglass reduction>".into()],
                    warnings: vec![],
                    clashes: vec![],
                },
            )
            .map_err(|e| anyhow!("the reduction pass failed to compile: {}", e.join("; ")))?;
            let histogram = crate::shader::link(
                gl,
                &format!("{v}#define BINS {BINS}.0\n{NONFINITE}{HISTOGRAM_VS}"),
                &format!("{v}{HISTOGRAM_FS}"),
                &["<shotglass histogram>".into()],
            )
            .map_err(|e| anyhow!("the histogram pass failed to compile: {}", e.join("; ")))?;
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let map = Target::new(gl, w, h, 1)?;
            let mut levels = vec![];
            let (mut lw, mut lh) = (w, h);
            while lw > 1 || lh > 1 {
                lw = (lw + 3) / 4;
                lh = (lh + 3) / 4;
                levels.push(Target::new(gl, lw, lh, 2)?);
            }
            let hist = Target::new(gl, BINS as i32, 1, 1)?;
            Ok(Gpu {
                vao,
                map,
                levels,
                hist,
                reduce,
                histogram,
            })
        }
    }

    pub unsafe fn delete(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.reduce);
            gl.delete_program(self.histogram);
            self.map.delete(gl);
            self.hist.delete(gl);
            for t in &self.levels {
                t.delete(gl);
            }
        }
    }

    /// Draw a metric into the map. `inputs` are bound to units 0.. in order and named
    /// by the matching sampler uniforms.
    pub unsafe fn draw_metric(
        &self,
        gl: &glow::Context,
        prog: glow::Program,
        inputs: &[(&str, glow::Texture)],
        info: &FrameInfo,
    ) {
        let (w, h) = self.map.size;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.map.fbo));
            gl.viewport(0, 0, w, h);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.use_program(Some(prog));
            gl.bind_vertex_array(Some(self.vao));
            let loc = |n: &str| gl.get_uniform_location(prog, n);
            for (unit, (name, tex)) in inputs.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_sampler(unit as u32, None);
                gl.bind_texture(glow::TEXTURE_2D, Some(*tex));
                gl.uniform_1_i32(loc(name).as_ref(), unit as i32);
            }
            gl.uniform_3_f32(loc("iResolution").as_ref(), w as f32, h as f32, 1.0);
            gl.uniform_1_f32(loc("iTime").as_ref(), info.t);
            gl.uniform_1_i32(loc("iFrame").as_ref(), info.frame);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            unbind(gl, inputs.len());
        }
    }

    /// Reduce the map down the levels: the last level's two texels.
    pub unsafe fn reduce(
        &self,
        gl: &glow::Context,
        threshold: Option<f64>,
    ) -> ([f32; 4], [f32; 4]) {
        unsafe {
            let prog = self.reduce;
            gl.use_program(Some(prog));
            gl.bind_vertex_array(Some(self.vao));
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            let loc = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_1_i32(loc("sg_src").as_ref(), 0);
            gl.uniform_1_i32(loc("sg_src2").as_ref(), 1);
            gl.uniform_1_f32(
                loc("sg_threshold").as_ref(),
                threshold.map_or(3.0e38, |t| t as f32),
            );
            let mut src = &self.map;
            for (i, dst) in self.levels.iter().enumerate() {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst.fbo));
                gl.viewport(0, 0, dst.size.0, dst.size.1);
                for (unit, tex) in [(0u32, src.tex[0]), (1, *src.tex.last().unwrap())] {
                    gl.active_texture(glow::TEXTURE0 + unit);
                    gl.bind_sampler(unit, None);
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                }
                gl.uniform_2_i32(loc("sg_size").as_ref(), src.size.0, src.size.1);
                gl.uniform_1_i32(loc("sg_first").as_ref(), (i == 0) as i32);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                src = dst;
            }
            unbind(gl, 2);
            if self.levels.is_empty() {
                // A 1×1 map: reduce on the CPU.
                let v = self.map.read(gl, 0).data()[0][3];
                return if v.is_finite() {
                    let over = threshold.is_some_and(|t| v as f64 > t) as u8 as f32;
                    ([v, v, 1.0, 0.0], [over, v, 0.0, 0.0])
                } else {
                    ([0.0, -3.0e38, 0.0, 1.0], [0.0, 3.0e38, 0.0, 0.0])
                };
            }
            (src.read(gl, 0).data()[0], src.read(gl, 1).data()[0])
        }
    }

    /// The map's finite measures counted into `BINS` bins over `range`.
    pub unsafe fn histogram(&self, gl: &glow::Context, range: (f64, f64)) -> Vec<u64> {
        let (w, h) = self.map.size;
        unsafe {
            let prog = self.histogram;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.hist.fbo));
            gl.viewport(0, 0, BINS as i32, 1);
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.enable(glow::BLEND);
            gl.blend_equation(glow::FUNC_ADD);
            gl.blend_func(glow::ONE, glow::ONE);
            // Desktop GL takes the point size from glPointSize unless this is on.
            gl.enable(glow::PROGRAM_POINT_SIZE);
            gl.use_program(Some(prog));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_sampler(0, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.map.tex[0]));
            let loc = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_1_i32(loc("sg_src").as_ref(), 0);
            gl.uniform_1_i32(loc("sg_width").as_ref(), w);
            gl.uniform_2_f32(loc("sg_range").as_ref(), range.0 as f32, range.1 as f32);
            gl.draw_arrays(glow::POINTS, 0, w * h);
            gl.disable(glow::PROGRAM_POINT_SIZE);
            gl.disable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ZERO); // GL's default
            unbind(gl, 1);
            self.hist
                .read(gl, 0)
                .data()
                .iter()
                .map(|t| t[0].max(0.0).round() as u64)
                .collect()
        }
    }
}

unsafe fn unbind(gl: &glow::Context, units: usize) {
    unsafe {
        gl.use_program(None);
        gl.bind_vertex_array(None);
        for unit in (0..units as u32).rev() {
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }
}
