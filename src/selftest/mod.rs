//! `shotglass selftest`: checks the renderer and the soundtrack on *this* GPU and
//! driver, using small fixture films written to a temporary folder.
//!
//! `cargo test` runs it too (tests/gpu.rs), so CI exercises it on software OpenGL
//! and you can run it on a real GPU after a driver update.

use crate::gfx::Tracked;
use crate::gfx::{Frame, FrameInfo, GlObjects, Renderer};
use crate::movie::{At, Movie};
use crate::shader::Dialect;
use crate::sound::Soundtrack;
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use toodee::{CopyOps, TooDeeOps, TooDeeOpsMut};

mod buffers;
mod comps;
mod export;
mod footage;
mod layers;
mod loops;
mod render;
mod sound;
mod support;
mod text;
mod textures;
mod tools;

use buffers::*;
use comps::*;
use export::*;
use footage::*;
use layers::*;
use loops::*;
use render::*;
use sound::*;
use support::*;
use text::*;
use textures::*;
use tools::*;

const W: i32 = 160;

const H: i32 = 90;

type Gl = Arc<eframe::glow::Context>;

struct Check {
    name: &'static str,
    run: fn(&Gl, &Path) -> Result<String>,
}

const CHECKS: &[Check] = &[
    Check {
        name: "transitions start on the old shot and end on the new one",
        run: transition_endpoints,
    },
    Check {
        name: "crossfade blends live and frozen shots correctly",
        run: crossfade_and_freeze,
    },
    Check {
        name: "reduced render scale has no edge bleed",
        run: render_scale_edges,
    },
    Check {
        name: "slow frames drawn in strips match a single draw",
        run: tiled_drawing,
    },
    Check {
        name: "readback into a larger frame lands in place and restores pack state",
        run: readback_into,
    },
    Check {
        name: "export YUV pass: BT.709 limited range, chroma sited left, read back pipelined",
        run: yuv_pass,
    },
    Check {
        name: "playback doesn't stall on driver error checks",
        run: error_check_rate,
    },
    Check {
        name: "compile errors point at the right file and line",
        run: error_mapping,
    },
    Check {
        name: "library files are included once, library transitions render, and errors point at name/file:line",
        run: library_errors,
    },
    Check {
        name: "a broken edit keeps the last working version on screen",
        run: keep_last_working,
    },
    Check {
        name: "program cache reloads identically and survives corruption",
        run: program_cache,
    },
    Check {
        name: "evicted programs come back correctly",
        run: eviction,
    },
    Check {
        name: "non-blocking render waits for compiles, then matches",
        run: lazy_compile,
    },
    Check {
        name: "shaders get the baseline #version, or the film's opt-in",
        run: version_line,
    },
    Check {
        name: "texture channels bind the right image to the right unit, upright as on Shadertoy",
        run: channel_binding,
    },
    Check {
        name: "texture filter, wrap, mipmaps and sRGB follow movie.toml; one upload per file",
        run: channel_sampling,
    },
    Check {
        name: "playback never blocks on a texture: uploads spread over frames, then match",
        run: texture_streaming,
    },
    Check {
        name: "a broken texture edit keeps the last working one; texture edits compile nothing",
        run: texture_edits,
    },
    Check {
        name: "evicted textures come back identically",
        run: texture_eviction,
    },
    Check {
        name: "frozen transitions use the outgoing shot's textures, and refreeze after an edit",
        run: texture_freeze,
    },
    Check {
        name: "texture uploads ignore and restore stray unpack state",
        run: texture_unpack_state,
    },
    Check {
        name: "iChannel4..7 bind like 0..3, with resolution and black when unbound",
        run: channels_high,
    },
    Check {
        name: "text textures: drawn without blocking, iChannelMeta reaches the shader, edits compile nothing",
        run: text_textures,
    },
    Check {
        name: "glyph runs: atlas and table reach the shader byte for byte, unflipped, without blocking",
        run: glyph_runs,
    },
    Check {
        name: "layers: source-over with opacity, @input, spans and fades",
        run: layers_compose,
    },
    Check {
        name: "layers: playback waits for every active layer; a broken layer edit keeps the last version",
        run: layers_ready_and_fallback,
    },
    Check {
        name: "layers: fixed scale ignores the render scale, auto follows it",
        run: layers_scale,
    },
    Check {
        name: "layers: blend passes, @picture and @<layer> (an earlier layer's own output)",
        run: layers_blend_and_refs,
    },
    Check {
        name: "layers: hidden layers (mute/solo) leave the picture, and their readers see black",
        run: layers_hidden,
    },
    Check {
        name: "comps: a comp shot equals the comp as a film; @<comp> at its size and time, held or looped",
        run: comps_render,
    },
    Check {
        name: "comps: playback waits for a comp; a broken comp item keeps its last version",
        run: comps_ready_and_fallback,
    },
    Check {
        name: "comps: footage inside a comp shows the exact frame of comp time",
        run: comps_footage,
    },
    Check {
        name: "comps: a live edit that removes a comp frees its targets",
        run: comps_removed_free,
    },
    Check {
        name: "comps: a frozen outgoing shot that reads a comp keeps its frame (iFrame)",
        run: comps_freeze,
    },
    Check {
        name: "mattes: alpha, luma, invert, a comp matte, a matted blend, an inactive source",
        run: mattes,
    },
    Check {
        name: "footage: sequence frames map exactly (24 in 30, in, speed, loop, black, iChannelTime, a layer's clock)",
        run: footage_mapping,
    },
    Check {
        name: "footage: playback never blocks on a frame and shows only the exact one",
        run: footage_streaming,
    },
    Check {
        name: "footage: cuts and loop seams are prefetched (no stall)",
        run: footage_prefetch,
    },
    Check {
        name: "footage: play, still and export agree, after seeking back and eviction",
        run: footage_agree,
    },
    Check {
        name: "footage: resident frames stay in the texture budget; textures are reused",
        run: footage_budget,
    },
    Check {
        name: "footage: a broken frame keeps the last working one; a missing frame refuses the edit",
        run: footage_broken,
    },
    Check {
        name: "footage: frames a buffer reads are prefetched (no stalls while playing)",
        run: footage_buffer_prefetch,
    },
    Check {
        name: "footage: frame uploads ignore and restore stray unpack state",
        run: footage_unpack_state,
    },
    Check {
        name: "footage: the repo's footage films play start to end without a prefetch miss",
        run: footage_films_no_misses,
    },
    Check {
        name: "buffers: every path agrees (forward, checkpoint, seeks, cold stills), bit for bit",
        run: buffers_agree,
    },
    Check {
        name: "buffers: a stall steps every missed frame before the frame is drawn",
        run: buffers_stalls,
    },
    Check {
        name: "buffers: playback never blocks; a far seek catches up in slices",
        run: buffers_playback,
    },
    Check {
        name: "buffers: the render scale doesn't touch their state",
        run: buffers_scale,
    },
    Check {
        name: "buffers: a reset (with an init pass) equals a fresh start",
        run: buffers_resets,
    },
    Check {
        name: "buffers: history frame = -k is the state k frames back; init before a reset; pure history needs no checkpoints",
        run: buffers_history,
    },
    Check {
        name: "buffers: edits to the scene, a texture or an upstream shot invalidate; a broken edit keeps the fold",
        run: buffers_edits,
    },
    Check {
        name: "buffers: progressive scrub is approximate at once, refines to the canonical frame, never headless; preview_lookback",
        run: buffers_scrub,
    },
    Check {
        name: "buffers: the background fill takes checkpoints a slice at a time, never blocking; cache = \"dense\"",
        run: buffers_fill,
    },
    Check {
        name: "buffers: an edit keeps the states before the first frame that reads it",
        run: buffers_invalidation,
    },
    Check {
        name: "buffers in comps: they fold over comp-local frames (offset, held, looped, read by a film buffer); every path agrees",
        run: buffers_in_comps,
    },
    Check {
        name: "buffers: loop = \"warm\" carries state over the seam (= an unlooped second period); paths agree; a wrap restores the seam",
        run: buffers_warm_loop,
    },
    Check {
        name: "buffers: after = \"<layer>\" steps after that layer (@input, @<layer>); paths agree; hidden layers and layers above don't touch it",
        run: buffers_after_layer,
    },
    Check {
        name: "GL objects: edits, resizes and removals return to the same counts; a dropped renderer frees everything",
        run: gl_object_churn,
    },
    Check {
        name: "chunked sound matches a single-pass render",
        run: sound_chunk_equivalence,
    },
    Check {
        name: "banded sound blocks match single draws",
        run: sound_banding,
    },
    Check {
        name: "streamed sound never waits for the GPU and matches the blocking render",
        run: sound_streaming,
    },
    Check {
        name: "sound has no clicks at cuts, fades and chunk edges",
        run: sound_no_clicks,
    },
    Check {
        name: "sound shader errors report the file and line and respect fallback timing",
        run: sound_errors,
    },
    Check {
        name: "export writes every frame and sample, matching still and wav",
        run: export_lossless,
    },
    Check {
        name: "shotglass test: assertions, property grids and failure reports",
        run: glsl_tests,
    },
    Check {
        name: "shotglass test: metric tests on PNG, GLSL and WAV inputs, expected failures",
        run: metric_tests,
    },
    Check {
        name: "loop films: the frame at t + k·period is the frame at t",
        run: loop_picture,
    },
    Check {
        name: "loop films: the soundtrack is seamless at the wrap and matches a long render",
        run: loop_sound,
    },
    Check {
        name: "quality metrics: @frame, @reference, GPU mean/max reductions, invalid, limits, heatmaps",
        run: quality_metrics,
    },
    Check {
        name: "quality metrics: @prev/@next (clamped at the ends), min, over, GPU histogram and percentiles",
        run: quality_temporal,
    },
    Check {
        name: "quality sound: BS.1770 loudness and true peak of a synthesised sine, sound-only config, limits",
        run: quality_sound,
    },
    Check {
        name: "quality sound metrics: a click across a block edge, its time, halo, partial last block",
        run: quality_clicks,
    },
];

pub fn run(filter: Option<String>) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("shotglass-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let dir2 = dir.clone();
    // GL errors fail the check that caused them (KHR_debug, where the driver has it).
    crate::gfx::gldebug::request(true);
    let result = crate::headless::with_gl(move |gl| {
        let r = Renderer::new(gl.clone())?;
        println!("GPU: {}", r.gl_info);
        println!(
            "parallel shader compile: {}",
            if r.parallel { "yes" } else { "no" }
        );
        println!(
            "GL debug output: {}",
            if crate::gfx::gldebug::active() {
                "on (errors fail the check)"
            } else {
                "not supported"
            }
        );
        drop(r);
        let mut failed = vec![];
        let mut ran = 0;
        for (i, c) in CHECKS.iter().enumerate() {
            if filter
                .as_ref()
                .is_some_and(|f| !c.name.contains(f.as_str()))
            {
                continue;
            }
            ran += 1;
            let sub = dir2.join(format!("check{i}"));
            std::fs::create_dir_all(&sub)?;
            let t = Instant::now();
            crate::gfx::glstate::set_audit(true);
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (c.run)(&gl, &sub)))
                .unwrap_or_else(|p| {
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Err(anyhow!("panicked: {msg}"))
                });
            // Whatever a check did, the GL state it leaves is the defaults the renderer
            // promises (gfx::glstate): a framebuffer, buffer, sampler or pixel-store
            // setting left behind leaks into the next pass, egui's or our own.
            let errors = crate::gfx::gldebug::take();
            let res = res.and_then(|note| {
                if errors.is_empty() {
                    Ok(note)
                } else {
                    Err(anyhow!("GL reported: {}", errors.join("; ")))
                }
            });
            let mut dirty = crate::gfx::glstate::take_audit();
            let end = crate::gfx::glstate::dirty(&gl);
            crate::gfx::glstate::reset(&gl);
            dirty.extend(end.into_iter().map(|x| format!("the check left {x}")));
            let res = res.and_then(|note| {
                if dirty.is_empty() {
                    Ok(note)
                } else {
                    Err(anyhow!("left GL state behind: {}", dirty.join(", ")))
                }
            });
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            let skipped = FOLD_NOTES.with(|n| std::mem::take(&mut *n.borrow_mut()));
            let res = res.map(|note| {
                if skipped.is_empty() {
                    note
                } else {
                    let s = format!(
                        "exact match skipped on {}: {} mismatches, first: {}",
                        inexact_sound(&gl).unwrap_or_default(),
                        skipped.len(),
                        skipped[0]
                    );
                    if note.is_empty() {
                        s
                    } else {
                        format!("{note}; {s}")
                    }
                }
            });
            match res {
                Ok(note) => {
                    println!(
                        "  ok    {} ({ms:.0} ms){}",
                        c.name,
                        if note.is_empty() {
                            String::new()
                        } else {
                            format!(" — {note}")
                        }
                    )
                }
                Err(e) => {
                    println!("  FAIL  {} ({ms:.0} ms)\n        {e:#}", c.name);
                    failed.push(c.name);
                }
            }
        }
        if failed.is_empty() {
            println!("\n{ran} checks passed");
            Ok(())
        } else {
            bail!("{} of {ran} checks failed", failed.len())
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}
