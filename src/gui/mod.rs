//! A small multi-window host for egui on top of winit + glutin + glow.
//!
//! eframe is not used because the console window needs raw physical key
//! events (modifiers, Super, PrintScreen, keypad, Ctrl+C, ...) that egui
//! swallows or does not model.

mod clipboard;
mod console;
mod manager;
mod playback;
mod vmedia;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use glutin::config::{Config as GlConfig, ConfigTemplateBuilder};
use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, PossiblyCurrentGlContext};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use winit::application::ApplicationHandler;
use winit::event::{KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowAttributes, WindowId};

pub use console::ConsoleWindow;
pub use manager::ManagerWindow;
pub use playback::PlaybackWindow;

#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Repaint a window, optionally after a delay.
    Repaint(WindowId, std::time::Duration),
    /// Ctrl+C or SIGTERM: close every window (logging out of the iDRACs)
    /// and exit.
    Quit,
}

/// Window attributes shared by all windows: icon and the application id
/// that matches `idrac6.desktop` (so docks group the windows correctly).
pub fn base_attributes() -> WindowAttributes {
    #[allow(unused_mut)]
    let mut a = Window::default_attributes().with_window_icon(app_icon());
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use winit::platform::wayland::WindowAttributesExtWayland;
        use winit::platform::x11::WindowAttributesExtX11;
        a = WindowAttributesExtWayland::with_name(a, "idrac6", "idrac6");
        a = WindowAttributesExtX11::with_name(a, "idrac6", "idrac6");
    }
    a
}

fn app_icon() -> Option<winit::window::Icon> {
    let img = image::load_from_memory(include_bytes!("../../assets/icon-64.png")).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Handed to window UIs so they can ask the shell for things.
pub struct Shell {
    /// Windows to open after this frame.
    pub open: Vec<Box<dyn WindowUi>>,
    /// Set to close the window that is currently being drawn.
    pub close_me: bool,
    pub quit: bool,
}

/// One kind of top-level window.
pub trait WindowUi {
    fn attributes(&self) -> WindowAttributes;
    /// Called once after the native window exists.
    fn created(&mut self, _window: &Window, _proxy: &EventLoopProxy<UserEvent>) {}
    fn ui(&mut self, ui: &mut egui::Ui, window: &Window, shell: &mut Shell);
    /// Give the window a chance to take a raw key event before egui sees it.
    /// Return true when consumed.
    fn raw_key(&mut self, _event: &KeyEvent) -> bool {
        false
    }
    fn focus_changed(&mut self, _focused: bool) {}
    /// Raw relative mouse motion from the device (not limited by the window
    /// edge). Return true to request a redraw.
    fn raw_mouse_motion(&mut self, _dx: f64, _dy: f64) -> bool {
        false
    }
    /// The user asked to close the window. Return false to keep it open.
    fn close_requested(&mut self) -> bool {
        true
    }
    /// Closing the main window quits the app.
    fn is_main(&self) -> bool {
        false
    }
}

// Fields drop in declaration order: the GL surface and context must go
// before the native window, or X11 reports GLXBadWindow on close.
struct GlWin {
    ui: Box<dyn WindowUi>,
    egui: egui_glow::EguiGlow,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    window: Arc<Window>,
    next_repaint: Option<Instant>,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    gl_config: Option<GlConfig>,
    windows: HashMap<WindowId, GlWin>,
    pending: Vec<Box<dyn WindowUi>>,
}

pub fn run(first: Box<dyn WindowUi>) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let quit = proxy.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = quit.send_event(UserEvent::Quit);
    }) {
        log::warn!("cannot handle Ctrl+C: {e}");
    }
    let mut app = App { proxy, gl_config: None, windows: HashMap::new(), pending: vec![first] };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl App {
    fn create_window(&mut self, el: &ActiveEventLoop, mut ui: Box<dyn WindowUi>) -> Result<()> {
        let attrs = ui.attributes().with_visible(false);
        let (window, config) = match &self.gl_config {
            Some(cfg) => (glutin_winit::finalize_window(el, attrs, cfg)?, cfg.clone()),
            None => {
                let template = ConfigTemplateBuilder::new().prefer_hardware_accelerated(None).with_depth_size(0).with_stencil_size(0).with_transparency(false);
                let (w, cfg) = glutin_winit::DisplayBuilder::new()
                    .with_preference(glutin_winit::ApiPreference::FallbackEgl)
                    .with_window_attributes(Some(attrs.clone()))
                    .build(el, template, |mut it| it.next().expect("no OpenGL config available"))
                    .map_err(|e| anyhow::anyhow!("creating OpenGL display: {e}"))?;
                let w = match w {
                    Some(w) => w,
                    None => glutin_winit::finalize_window(el, attrs, &cfg)?,
                };
                self.gl_config = Some(cfg.clone());
                (w, cfg)
            }
        };
        let display = config.display();
        let raw = window.window_handle().ok().map(|h| h.as_raw());
        let ctx_attrs = ContextAttributesBuilder::new().build(raw);
        let gles_attrs = ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(None)).build(raw);
        // SAFETY: the window handle stays valid for the lifetime of the context.
        let not_current = unsafe { display.create_context(&config, &ctx_attrs).or_else(|_| display.create_context(&config, &gles_attrs)) }
            .context("creating OpenGL context")?;
        let (w, h): (u32, u32) = window.inner_size().into();
        let sattrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            window.window_handle()?.as_raw(),
            NonZeroU32::new(w).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(h).unwrap_or(NonZeroU32::MIN),
        );
        // SAFETY: as above.
        let surface = unsafe { display.create_window_surface(&config, &sattrs)? };
        let context = not_current.make_current(&surface)?;
        let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN));
        // SAFETY: the loader comes from the display that owns the current context.
        let gl = Arc::new(unsafe {
            glow::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s).unwrap();
                display.get_proc_address(&s)
            })
        });
        let egui = egui_glow::EguiGlow::new(el, gl, None, None, true);
        let id = window.id();
        let proxy = self.proxy.clone();
        egui.egui_ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(UserEvent::Repaint(id, info.delay));
        });
        ui.created(&window, &self.proxy);
        window.set_visible(true);
        window.request_redraw();
        self.windows.insert(id, GlWin { window: Arc::new(window), surface, context, egui, ui, next_repaint: None });
        Ok(())
    }

    fn redraw(&mut self, el: &ActiveEventLoop, id: WindowId) {
        let Some(w) = self.windows.get_mut(&id) else { return };
        if let Err(e) = w.context.make_current(&w.surface) {
            log::error!("make_current: {e}");
            return;
        }
        let mut shell = Shell { open: Vec::new(), close_me: false, quit: false };
        let window = w.window.clone();
        let ui = &mut w.ui;
        w.egui.run(&window, |eui| ui.ui(eui, &window, &mut shell));
        // SAFETY: the context is current.
        unsafe {
            use glow::HasContext;
            let gl = w.egui.painter.gl();
            gl.clear_color(0.08, 0.08, 0.09, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        w.egui.paint(&window);
        if let Err(e) = w.surface.swap_buffers(&w.context) {
            log::warn!("swap_buffers: {e}");
        }
        self.pending.extend(shell.open);
        if shell.quit {
            el.exit();
        }
        if shell.close_me {
            self.close(el, id);
        }
    }

    fn close(&mut self, el: &ActiveEventLoop, id: WindowId) {
        if let Some(mut w) = self.windows.remove(&id) {
            let _ = w.context.make_current(&w.surface);
            w.egui.destroy();
            if w.ui.is_main() {
                el.exit();
            }
        }
        if self.windows.is_empty() {
            el.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        for ui in std::mem::take(&mut self.pending) {
            if let Err(e) = self.create_window(el, ui) {
                log::error!("could not create window: {e:#}");
                eprintln!("error: could not create window: {e:#}");
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(w) = self.windows.get_mut(&id) else { return };
        match &event {
            WindowEvent::CloseRequested => {
                if w.ui.close_requested() {
                    self.close(el, id);
                }
                return;
            }
            WindowEvent::RedrawRequested => {
                w.next_repaint = None;
                self.redraw(el, id);
                return;
            }
            WindowEvent::Resized(size) => {
                if let (Some(wd), Some(ht)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                    let _ = w.context.make_current(&w.surface);
                    w.surface.resize(&w.context, wd, ht);
                }
            }
            WindowEvent::Focused(f) => w.ui.focus_changed(*f),
            WindowEvent::KeyboardInput { event: ke, .. } => {
                if w.ui.raw_key(ke) {
                    return;
                }
            }
            _ => {}
        }
        let resp = w.egui.on_window_event(&w.window, &event);
        if resp.repaint {
            w.window.request_redraw();
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, event: winit::event::DeviceEvent) {
        if let winit::event::DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            for w in self.windows.values_mut() {
                if w.ui.raw_mouse_motion(dx, dy) {
                    w.window.request_redraw();
                }
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Quit => {
                log::info!("quitting on signal");
                for w in self.windows.values_mut() {
                    w.ui.close_requested();
                }
                el.exit();
            }
            UserEvent::Repaint(id, delay) => {
                if let Some(w) = self.windows.get_mut(&id) {
                    if delay.is_zero() {
                        w.window.request_redraw();
                    } else if let Some(at) = Instant::now().checked_add(delay) {
                        w.next_repaint = Some(w.next_repaint.map_or(at, |t| t.min(at)));
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if !self.pending.is_empty() {
            for ui in std::mem::take(&mut self.pending) {
                if let Err(e) = self.create_window(el, ui) {
                    log::error!("could not create window: {e:#}");
                }
            }
        }
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        for w in self.windows.values_mut() {
            if let Some(t) = w.next_repaint {
                if t <= now {
                    w.next_repaint = None;
                    w.window.request_redraw();
                } else {
                    next = Some(next.map_or(t, |n| n.min(t)));
                }
            }
        }
        el.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        for (_, mut w) in self.windows.drain() {
            let _ = w.context.make_current(&w.surface);
            w.egui.destroy();
        }
    }
}
