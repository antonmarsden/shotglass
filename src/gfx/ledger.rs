//! A ledger of the GL objects shotglass creates and deletes.
//!
//! `glow` handles are plain numbers: dropping one from a `Vec` leaks the GPU object
//! without a trace. Every create and delete in the crate goes through [`Tracked`],
//! which counts live objects per kind in process-wide atomics, so the selftest can
//! check that a renderer frees everything it made, and that edits, resizes and
//! removals return to the same counts (selftest "GL objects: …").
//!
//! The counts are cheap (one relaxed atomic per create or delete, never a driver
//! round trip) and cover only objects made through this trait: egui's own objects
//! aren't in them.

use eframe::glow::{self, HasContext};
use std::sync::atomic::{AtomicI64, Ordering};

/// The kinds counted, in [`GlObjects::counts`] order.
pub const KINDS: [&str; 9] = [
    "textures",
    "framebuffers",
    "buffers",
    "programs",
    "shaders",
    "samplers",
    "queries",
    "vertex arrays",
    "fences",
];

const TEX: usize = 0;
const FBO: usize = 1;
const BUF: usize = 2;
const PROG: usize = 3;
const SHADER: usize = 4;
const SAMPLER: usize = 5;
const QUERY: usize = 6;
const VAO: usize = 7;
const FENCE: usize = 8;

static LIVE: [AtomicI64; 9] = [const { AtomicI64::new(0) }; 9];

fn add(kind: usize, d: i64) {
    LIVE[kind].fetch_add(d, Ordering::Relaxed);
}

/// Live GL objects per kind (created minus deleted through [`Tracked`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlObjects {
    pub counts: [i64; 9],
}

impl GlObjects {
    /// Now, process-wide.
    pub fn live() -> GlObjects {
        GlObjects {
            counts: std::array::from_fn(|k| LIVE[k].load(Ordering::Relaxed)),
        }
    }

    /// `self − before` per kind, as "textures +2, framebuffers +2" (empty if equal).
    pub fn diff(&self, before: &GlObjects) -> String {
        (0..KINDS.len())
            .filter_map(|k| {
                let d = self.counts[k] - before.counts[k];
                (d != 0).then(|| format!("{} {d:+}", KINDS[k]))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

macro_rules! pair {
    ($new:ident, $free:ident, $create:ident, $delete:ident, $t:ty, $kind:expr) => {
        /// Like `glow`'s create, counted in the ledger.
        ///
        /// # Safety
        /// As the `glow` call it wraps.
        unsafe fn $new(&self) -> Result<$t, String>;
        /// Like `glow`'s delete, counted in the ledger.
        ///
        /// # Safety
        /// As the `glow` call it wraps.
        unsafe fn $free(&self, x: $t);
    };
}

macro_rules! pair_impl {
    ($new:ident, $free:ident, $create:ident, $delete:ident, $t:ty, $kind:expr) => {
        unsafe fn $new(&self) -> Result<$t, String> {
            let x = unsafe { self.$create() }?;
            add($kind, 1);
            Ok(x)
        }
        unsafe fn $free(&self, x: $t) {
            unsafe { self.$delete(x) };
            add($kind, -1);
        }
    };
}

/// Counted creates and deletes. Use these, never `glow`'s `create_*`/`delete_*`.
pub trait Tracked {
    pair!(
        new_texture,
        free_texture,
        create_texture,
        delete_texture,
        glow::Texture,
        TEX
    );
    pair!(
        new_framebuffer,
        free_framebuffer,
        create_framebuffer,
        delete_framebuffer,
        glow::Framebuffer,
        FBO
    );
    pair!(
        new_buffer,
        free_buffer,
        create_buffer,
        delete_buffer,
        glow::Buffer,
        BUF
    );
    pair!(
        new_program,
        free_program,
        create_program,
        delete_program,
        glow::Program,
        PROG
    );
    pair!(
        new_sampler,
        free_sampler,
        create_sampler,
        delete_sampler,
        glow::Sampler,
        SAMPLER
    );
    pair!(
        new_query,
        free_query,
        create_query,
        delete_query,
        glow::Query,
        QUERY
    );
    pair!(
        new_vertex_array,
        free_vertex_array,
        create_vertex_array,
        delete_vertex_array,
        glow::VertexArray,
        VAO
    );

    /// Like `glow`'s `create_shader`, counted in the ledger.
    ///
    /// # Safety
    /// As the `glow` call it wraps.
    unsafe fn new_shader(&self, kind: u32) -> Result<glow::Shader, String>;
    /// Like `glow`'s `delete_shader`, counted in the ledger.
    ///
    /// # Safety
    /// As the `glow` call it wraps.
    unsafe fn free_shader(&self, x: glow::Shader);
    /// Like `glow`'s `fence_sync`, counted in the ledger.
    ///
    /// # Safety
    /// As the `glow` call it wraps.
    unsafe fn new_fence(&self, condition: u32, flags: u32) -> Result<glow::Fence, String>;
    /// Like `glow`'s `delete_sync`, counted in the ledger.
    ///
    /// # Safety
    /// As the `glow` call it wraps.
    unsafe fn free_fence(&self, x: glow::Fence);
}

impl Tracked for glow::Context {
    pair_impl!(
        new_texture,
        free_texture,
        create_texture,
        delete_texture,
        glow::Texture,
        TEX
    );
    pair_impl!(
        new_framebuffer,
        free_framebuffer,
        create_framebuffer,
        delete_framebuffer,
        glow::Framebuffer,
        FBO
    );
    pair_impl!(
        new_buffer,
        free_buffer,
        create_buffer,
        delete_buffer,
        glow::Buffer,
        BUF
    );
    pair_impl!(
        new_program,
        free_program,
        create_program,
        delete_program,
        glow::Program,
        PROG
    );
    pair_impl!(
        new_sampler,
        free_sampler,
        create_sampler,
        delete_sampler,
        glow::Sampler,
        SAMPLER
    );
    pair_impl!(
        new_query,
        free_query,
        create_query,
        delete_query,
        glow::Query,
        QUERY
    );
    pair_impl!(
        new_vertex_array,
        free_vertex_array,
        create_vertex_array,
        delete_vertex_array,
        glow::VertexArray,
        VAO
    );

    unsafe fn new_shader(&self, kind: u32) -> Result<glow::Shader, String> {
        let x = unsafe { self.create_shader(kind) }?;
        add(SHADER, 1);
        Ok(x)
    }
    unsafe fn free_shader(&self, x: glow::Shader) {
        unsafe { self.delete_shader(x) };
        add(SHADER, -1);
    }
    unsafe fn new_fence(&self, condition: u32, flags: u32) -> Result<glow::Fence, String> {
        let x = unsafe { self.fence_sync(condition, flags) }?;
        add(FENCE, 1);
        Ok(x)
    }
    unsafe fn free_fence(&self, x: glow::Fence) {
        unsafe { self.delete_sync(x) };
        add(FENCE, -1);
    }
}

#[cfg(test)]
mod tests {
    /// Every create and delete goes through `Tracked`, or the ledger lies: no source
    /// file but this one calls glow's own (validate.rs's `create_shader` is glslang's).
    #[test]
    fn no_untracked_gl_objects() {
        let raw = raw_calls();
        let mut bad = vec![];
        for e in walk(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))) {
            let name = e.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            if name.ends_with("gfx/ledger.rs") || name.ends_with("/validate.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&e).unwrap();
            for (i, line) in text.lines().enumerate() {
                if raw.iter().any(|r| line.contains(r)) {
                    bad.push(format!("{name}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "untracked GL creates/deletes (use gfx::Tracked):\n{}",
            bad.join("\n")
        );
    }

    fn raw_calls() -> Vec<String> {
        let kinds = [
            "texture",
            "framebuffer",
            "buffer",
            "program",
            "shader",
            "sampler",
            "query",
            "vertex_array",
            "renderbuffer",
        ];
        let mut v: Vec<String> = kinds
            .iter()
            .flat_map(|k| [format!(".create_{k}("), format!(".delete_{k}(")])
            .collect();
        v.extend([".fence_sync(".into(), ".delete_sync(".into()]);
        v
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
        out
    }
}
