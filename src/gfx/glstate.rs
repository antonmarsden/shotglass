//! The GL state shotglass promises to leave behind (tests only).
//!
//! The renderer shares one context with egui and with itself across passes, so
//! anything left bound leaks into whatever draws next: a framebuffer the player's UI
//! paints into (#138), a `PIXEL_UNPACK_BUFFER` a client-memory upload reads an offset
//! of, a sampler object overriding egui's filtering, a pack row length a readback
//! honours. [`dirty`] reads the state back and names what isn't at its default.
//!
//! Every read is a driver round trip: never call this on the render path. The
//! selftest "GL state …" calls it after each kind of work.

use eframe::glow::{self, HasContext};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static AUDIT: AtomicBool = AtomicBool::new(false);
static FOUND: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Tests: have the renderer's public entry points (`render_at`, `pump`, the audio
/// calls, readbacks, `fill_checkpoints`) check the state on return. Process-wide.
pub fn set_audit(on: bool) {
    AUDIT.store(on, Ordering::Relaxed);
}

/// What the audit found since the last call ("render_at left X = …"), cleared.
pub fn take_audit() -> Vec<String> {
    std::mem::take(&mut *FOUND.lock().unwrap())
}

/// Called on return from a renderer entry point: records (once per entry point and
/// setting) and resets dirty state, so one leak reads as one finding.
pub(super) fn audit(gl: &glow::Context, what: &str) {
    if !AUDIT.load(Ordering::Relaxed) {
        return;
    }
    let d = dirty(gl);
    if d.is_empty() {
        return;
    }
    reset(gl);
    let mut f = FOUND.lock().unwrap();
    for x in d {
        let line = format!("{what} left {x}");
        if !f.contains(&line) {
            f.push(line);
        }
    }
}

/// Texture units the engine binds samplers on: the channels and engine units above
/// them (layers: 8–9, mattes …): checked to 16.
const SAMPLER_UNITS: u32 = 16;

/// What isn't at its default, as "NAME = got (want)" (empty: clean).
pub fn dirty(gl: &glow::Context) -> Vec<String> {
    let es = gl.version().is_embedded;
    let mut out = vec![];
    let int = |pname: u32| unsafe { gl.get_parameter_i32(pname) };
    let mut want = |name: &str, pname: u32, default: i32| {
        let got = int(pname);
        if got != default {
            out.push(format!("{name} = {got} ({default})"));
        }
    };
    want(
        "DRAW_FRAMEBUFFER_BINDING",
        glow::DRAW_FRAMEBUFFER_BINDING,
        0,
    );
    want(
        "READ_FRAMEBUFFER_BINDING",
        glow::READ_FRAMEBUFFER_BINDING,
        0,
    );
    want(
        "PIXEL_PACK_BUFFER_BINDING",
        glow::PIXEL_PACK_BUFFER_BINDING,
        0,
    );
    want(
        "PIXEL_UNPACK_BUFFER_BINDING",
        glow::PIXEL_UNPACK_BUFFER_BINDING,
        0,
    );
    want("UNPACK_ALIGNMENT", glow::UNPACK_ALIGNMENT, 4);
    want("UNPACK_ROW_LENGTH", glow::UNPACK_ROW_LENGTH, 0);
    want("UNPACK_SKIP_ROWS", glow::UNPACK_SKIP_ROWS, 0);
    want("UNPACK_SKIP_PIXELS", glow::UNPACK_SKIP_PIXELS, 0);
    want("PACK_ALIGNMENT", glow::PACK_ALIGNMENT, 4);
    want("PACK_ROW_LENGTH", glow::PACK_ROW_LENGTH, 0);
    want("PACK_SKIP_ROWS", glow::PACK_SKIP_ROWS, 0);
    want("PACK_SKIP_PIXELS", glow::PACK_SKIP_PIXELS, 0);
    if !es {
        want("PACK_SWAP_BYTES", glow::PACK_SWAP_BYTES, 0);
    }
    want(
        "ACTIVE_TEXTURE",
        glow::ACTIVE_TEXTURE,
        glow::TEXTURE0 as i32,
    );
    let scissor = unsafe { gl.is_enabled(glow::SCISSOR_TEST) };
    if scissor {
        out.push("SCISSOR_TEST enabled (disabled)".into());
    }
    let active = int(glow::ACTIVE_TEXTURE) as u32;
    for u in 0..SAMPLER_UNITS {
        unsafe { gl.active_texture(glow::TEXTURE0 + u) };
        let s = int(glow::SAMPLER_BINDING);
        if s != 0 {
            out.push(format!("SAMPLER_BINDING on unit {u} = {s} (0)"));
        }
    }
    unsafe { gl.active_texture(active) };
    out
}

/// Put back every default [`dirty`] checks (so one failing check doesn't fail the next).
pub fn reset(gl: &glow::Context) {
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        for (p, v) in [
            (glow::UNPACK_ALIGNMENT, 4),
            (glow::UNPACK_ROW_LENGTH, 0),
            (glow::UNPACK_SKIP_ROWS, 0),
            (glow::UNPACK_SKIP_PIXELS, 0),
            (glow::PACK_ALIGNMENT, 4),
            (glow::PACK_ROW_LENGTH, 0),
            (glow::PACK_SKIP_ROWS, 0),
            (glow::PACK_SKIP_PIXELS, 0),
        ] {
            gl.pixel_store_i32(p, v);
        }
        if !gl.version().is_embedded {
            gl.pixel_store_i32(glow::PACK_SWAP_BYTES, 0);
        }
        for u in 0..SAMPLER_UNITS {
            gl.bind_sampler(u, None);
        }
        gl.active_texture(glow::TEXTURE0);
        gl.disable(glow::SCISSOR_TEST);
    }
}
