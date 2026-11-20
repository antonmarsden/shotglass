//! The web player (wasm32): download a movie over HTTP, then play it on a canvas
//! with WebGL 2 and WebAudio.
//!
//! `index.html?movie=<url>` names the movie folder (or its `movie.toml`), relative
//! to the page or absolute. Every file the movie needs — the ones `movie.toml`
//! names and everything they `#include` — is fetched before the Play button
//! appears; the player then reads them from memory, exactly as the desktop reads
//! them from disk. Another origin must send CORS headers (see README).

use crate::app::{self, MovieSource};
use crate::audio::AudioOut;
use crate::files::{Memory, Preloader};
use crate::movie::Movie;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// The movie played when the page has no `?movie=` (trunk copies the demo here).
const DEFAULT_MOVIE: &str = "big-shadow/";

pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("shotglass crashed: {info}");
        web_sys::console::error_1(&msg.clone().into());
        status(&msg);
    }));
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = run().await {
            web_sys::console::error_1(&e.clone().into());
            status(&e);
        }
    });
}

async fn run() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let doc = window.document().ok_or("no document")?;
    let page = window.location().href().map_err(js)?;
    let search = window.location().search().map_err(js)?;
    let movie_param = web_sys::UrlSearchParams::new_with_str(&search)
        .map_err(js)?
        .get("movie")
        .unwrap_or_else(|| DEFAULT_MOVIE.into());
    let toml_url = toml_url(&movie_param, &page)?;

    status(&format!("Loading {toml_url}"));
    let movie = download(&toml_url).await?;

    let button: web_sys::HtmlButtonElement = element(&doc, "play")?;
    status(&format!(
        "{} · {:.1} s · {} shots",
        movie.title,
        movie.duration,
        movie.shots.len()
    ));
    button.set_disabled(false);

    // Browsers only allow audio to start inside a user gesture, so the AudioContext
    // is created in the click handler itself.
    let audio: Rc<RefCell<Option<anyhow::Result<AudioOut>>>> = Rc::default();
    let clicked = js_sys::Promise::new(&mut |resolve, _reject| {
        let audio = audio.clone();
        let on_click = Closure::once_into_js(move || {
            *audio.borrow_mut() = Some(AudioOut::open());
            let _ = resolve.call0(&JsValue::NULL);
        });
        button.set_onclick(Some(on_click.unchecked_ref()));
    });
    JsFuture::from(clicked).await.map_err(js)?;
    button.set_onclick(None);
    let audio = audio
        .borrow_mut()
        .take()
        .ok_or("the Play button gave no audio")?;

    if let Some(overlay) = doc.get_element_by_id("start") {
        overlay.set_attribute("hidden", "").map_err(js)?;
    }
    let canvas: web_sys::HtmlCanvasElement = element(&doc, "player")?;
    let source = MovieSource::Loaded {
        movie: Box::new(movie),
        label: toml_url,
    };
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |cc| {
                app::new_player(cc, source, audio).map_err(|e| format!("{e:#}").into())
            }),
        )
        .await
        .map_err(js)
}

/// Fetch `movie.toml`, then every file it needs, following `#include`s.
async fn download(toml_url: &str) -> Result<Movie, String> {
    let toml = fetch_text(toml_url)
        .await?
        .ok_or_else(|| format!("{toml_url}: not found (404)"))?;
    // The movie root is "" and paths are relative to it, so a path maps to a URL
    // by resolving it against movie.toml's URL.
    let file = Path::new("movie.toml");
    let mut first = Memory::default();
    first.insert(file, toml.clone());
    let named = Movie::load_from(Arc::new(first), file).map_err(|e| format!("{e:#}"))?;

    let mut pre = Preloader::new(&named.root, named.referenced_files());
    pre.insert(file, toml);
    let mut fetched = 0;
    loop {
        let want = pre.wanted();
        if want.is_empty() {
            break;
        }
        // Start every request of this round before awaiting any of them.
        let mut pending = vec![];
        for p in &want {
            let url = file_url(p, toml_url)?;
            pending.push((p, url.clone(), start_fetch(&url)?));
        }
        for (p, url, req) in pending {
            status(&format!("Downloading {} ({fetched} files)", rel(p)));
            pre.provide(p, finish_fetch(&url, req).await?);
            fetched += 1;
        }
    }
    for m in &pre.missing {
        web_sys::console::log_1(&format!("not on the server: {}", rel(m)).into());
    }
    // Missing files are reported by the player like on the desktop (with file:line
    // for includes).
    Movie::load_from(Arc::new(pre.finish()), file).map_err(|e| format!("{e:#}"))
}

/// `?movie=` may name a folder or a `.toml` file, relative to the page.
fn toml_url(movie: &str, page: &str) -> Result<String, String> {
    let movie = if movie.ends_with(".toml") {
        movie.to_string()
    } else if movie.ends_with('/') {
        format!("{movie}movie.toml")
    } else {
        format!("{movie}/movie.toml")
    };
    Ok(web_sys::Url::new_with_base(&movie, page)
        .map_err(|_| format!("?movie={movie} isn't a valid URL"))?
        .href())
}

fn file_url(p: &Path, toml_url: &str) -> Result<String, String> {
    Ok(web_sys::Url::new_with_base(&rel(p), toml_url)
        .map_err(js)?
        .href())
}

fn rel(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn start_fetch(url: &str) -> Result<JsFuture, String> {
    let window = web_sys::window().ok_or("no window")?;
    Ok(JsFuture::from(window.fetch_with_str(url)))
}

/// The body of a fetch, `None` for 404, an error for anything else that failed.
async fn finish_fetch(url: &str, req: JsFuture) -> Result<Option<String>, String> {
    let resp: web_sys::Response = req
        .await
        .map_err(|_| {
            format!("couldn't fetch {url} (offline, or another site without CORS headers?)")
        })?
        .dyn_into()
        .map_err(js)?;
    if resp.status() == 404 {
        return Ok(None);
    }
    if !resp.ok() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    let text = JsFuture::from(resp.text().map_err(js)?).await.map_err(js)?;
    Ok(text.as_string())
}

async fn fetch_text(url: &str) -> Result<Option<String>, String> {
    finish_fetch(url, start_fetch(url)?).await
}

fn element<T: JsCast>(doc: &web_sys::Document, id: &str) -> Result<T, String> {
    doc.get_element_by_id(id)
        .ok_or_else(|| format!("index.html has no #{id}"))?
        .dyn_into()
        .map_err(|_| format!("#{id} has the wrong element type"))
}

/// Show a line of text on the start screen.
fn status(s: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("status"))
    {
        el.set_text_content(Some(s));
    }
}

fn js(e: JsValue) -> String {
    format!("{e:?}")
}

/// Draws the player's output texture into the egui view. On the desktop eframe
/// registers our GL texture as an egui image; its web backend can't, so here the
/// texture is drawn with a paint callback instead (same picture, no CPU copy).
pub struct Blit {
    prog: eframe::glow::Program,
    vao: eframe::glow::VertexArray,
}

const BLIT_VS: &str = "#version 300 es
out vec2 uv;
void main() {
    vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
    uv = p * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
";

const BLIT_FS: &str = "#version 300 es
precision highp float;
uniform sampler2D tex;
in vec2 uv;
out vec4 color;
void main() { color = vec4(texture(tex, uv).rgb, 1.0); }
";

impl Blit {
    pub fn new(gl: &eframe::glow::Context) -> Result<Blit, String> {
        use eframe::glow::HasContext;
        unsafe {
            let prog = gl.create_program()?;
            let mut shaders = vec![];
            for (kind, src) in [
                (eframe::glow::VERTEX_SHADER, BLIT_VS),
                (eframe::glow::FRAGMENT_SHADER, BLIT_FS),
            ] {
                let sh = gl.create_shader(kind)?;
                gl.shader_source(sh, src);
                gl.compile_shader(sh);
                gl.attach_shader(prog, sh);
                shaders.push(sh);
            }
            gl.link_program(prog);
            if !gl.get_program_link_status(prog) {
                return Err(format!("blit: {}", gl.get_program_info_log(prog)));
            }
            for sh in shaders {
                gl.detach_shader(prog, sh);
                gl.delete_shader(sh);
            }
            Ok(Blit {
                prog,
                vao: gl.create_vertex_array()?,
            })
        }
    }

    /// A paint command that draws `tex` (bottom row first, as GL renders it) over `rect`.
    pub fn paint(
        &self,
        rect: eframe::egui::Rect,
        tex: eframe::glow::Texture,
    ) -> eframe::egui::PaintCallback {
        let (prog, vao) = (self.prog, self.vao);
        eframe::egui::PaintCallback {
            rect,
            callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_info, painter| {
                use eframe::glow::{self, HasContext};
                let gl = painter.gl();
                // egui_glow has set the viewport to `rect` and restores its state after.
                unsafe {
                    gl.disable(glow::BLEND);
                    gl.use_program(Some(prog));
                    gl.bind_vertex_array(Some(vao));
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                    gl.bind_vertex_array(None);
                }
            })),
        }
    }
}
