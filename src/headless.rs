//! An OpenGL context without a visible window, for `still`, `strip`, `check` and `wav`.

use anyhow::{Result, anyhow};
use eframe::glow;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::GlSurface;
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

type Job<R> = Box<dyn FnOnce(Arc<glow::Context>) -> Result<R>>;

struct Runner<R> {
    job: Option<Job<R>>,
    result: Option<Result<R>>,
}

impl<R> ApplicationHandler for Runner<R> {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if let Some(job) = self.job.take() {
            self.result = Some(run(el, job));
            el.exit();
        }
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn run<R>(el: &ActiveEventLoop, job: Job<R>) -> Result<R> {
    let attrs = Window::default_attributes()
        .with_title("shotglass (headless)")
        .with_visible(false)
        .with_inner_size(winit::dpi::PhysicalSize::new(64, 64));
    let (window, config) = DisplayBuilder::new()
        .with_window_attributes(Some(attrs))
        .build(el, ConfigTemplateBuilder::new(), |mut configs| {
            configs.next().unwrap()
        })
        .map_err(|e| anyhow!("creating a GL config: {e}"))?;
    let window = window.ok_or_else(|| anyhow!("no window"))?;
    let display = config.display();
    let handle = window.window_handle().ok().map(|h| h.as_raw());
    let contexts = [
        ContextAttributesBuilder::new().build(handle),
        ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(None))
            .build(handle),
    ];
    let mut not_current = None;
    let mut last_err = None;
    for c in &contexts {
        match unsafe { display.create_context(&config, c) } {
            Ok(ctx) => {
                not_current = Some(ctx);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let not_current = not_current.ok_or_else(|| anyhow!("creating a GL context: {last_err:?}"))?;
    let surface_attrs = window.build_surface_attributes(Default::default())?;
    let surface = unsafe { display.create_window_surface(&config, &surface_attrs)? };
    let context = not_current.make_current(&surface)?;
    let _ = surface.swap_buffers(&context);
    let mut gl =
        unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
    crate::gfx::gldebug::install(&mut gl);
    let gl = Arc::new(gl);
    let out = job(gl);
    drop(context);
    drop(surface);
    drop(window);
    out
}

/// Run `job` with a current OpenGL context.
pub fn with_gl<R: 'static>(
    job: impl FnOnce(Arc<glow::Context>) -> Result<R> + 'static,
) -> Result<R> {
    // winit panics (rather than erroring) when an X11 library is missing: report it.
    let el = std::panic::catch_unwind(EventLoop::new)
        .map_err(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            anyhow!("can't open a display for OpenGL: {msg} (on Linux, install libxkbcommon-x11-0)")
        })?
        .map_err(|e| anyhow!("can't open a display for OpenGL: {e}"))?;
    let mut runner = Runner {
        job: Some(Box::new(job)),
        result: None,
    };
    el.run_app(&mut runner)?;
    runner
        .result
        .unwrap_or_else(|| Err(anyhow!("the event loop ended before rendering")))
}
