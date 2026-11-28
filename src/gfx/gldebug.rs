//! GL debug output (KHR_debug / GL 4.3) for tests.
//!
//! The selftest turns this on before the headless context is made; the context then
//! reports errors synchronously (an invalid enum, a bad bind, a delete of something
//! that isn't an object, a draw with an incomplete framebuffer) to a callback that
//! records them, and the runner fails the check they came from. Drivers that don't
//! offer the extension (the Apple Software Renderer's GL 4.1) just don't report.
//!
//! Synchronous debug output slows the driver: it's never on outside tests.

use eframe::glow::{self, HasContext};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static WANT: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static FOUND: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Messages kept per check (a broken loop would report one per call).
const KEEP: usize = 8;

/// Ask for debug output in contexts made from now on (`headless::with_gl`).
pub fn request(on: bool) {
    WANT.store(on, Ordering::Relaxed);
}

/// Did the current context install the callback (the driver supports KHR_debug)?
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Install the callback on a fresh context, if requested and supported.
pub fn install(gl: &mut glow::Context) {
    if !WANT.load(Ordering::Relaxed) || !gl.supports_debug() {
        return;
    }
    unsafe {
        gl.enable(glow::DEBUG_OUTPUT);
        gl.enable(glow::DEBUG_OUTPUT_SYNCHRONOUS);
        gl.debug_message_callback(|source, kind, id, severity, msg| {
            if !counts(source, kind, severity) {
                return;
            }
            let mut f = FOUND.lock().unwrap_or_else(|e| e.into_inner());
            if f.len() < KEEP {
                f.push(format!(
                    "{} (source {source:#x}, id {id}): {}",
                    kind_name(kind),
                    msg.trim()
                ));
            }
        });
    }
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Errors and undefined behaviour count, whatever the severity; other kinds only at
/// high severity (performance notes and NVIDIA's buffer placement info don't). The
/// shader compiler's messages never do: Mesa reports every compile error and warning
/// here too, and those reach the user through the info log (`remap_log`), which
/// checks deliberately exercise with broken shaders.
fn counts(source: u32, kind: u32, severity: u32) -> bool {
    source != glow::DEBUG_SOURCE_SHADER_COMPILER
        && (matches!(
            kind,
            glow::DEBUG_TYPE_ERROR | glow::DEBUG_TYPE_UNDEFINED_BEHAVIOR
        ) || severity == glow::DEBUG_SEVERITY_HIGH)
}

fn kind_name(kind: u32) -> &'static str {
    match kind {
        glow::DEBUG_TYPE_ERROR => "GL error",
        glow::DEBUG_TYPE_UNDEFINED_BEHAVIOR => "undefined behaviour",
        glow::DEBUG_TYPE_DEPRECATED_BEHAVIOR => "deprecated behaviour",
        glow::DEBUG_TYPE_PORTABILITY => "portability",
        glow::DEBUG_TYPE_PERFORMANCE => "performance",
        _ => "GL message",
    }
}

/// What was reported since the last call, cleared.
pub fn take() -> Vec<String> {
    std::mem::take(&mut *FOUND.lock().unwrap_or_else(|e| e.into_inner()))
}
