//! Native window output (fullscreen by default): the screensaver mode.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::engine::{Engine, EngineConfig};
use crate::render::{Gpu, Renderer, instance};

pub struct WindowOptions {
    pub fullscreen: bool,
    pub fps: f64,
    pub stars: f32,
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

struct App {
    cfg: EngineConfig,
    opts: WindowOptions,
    gfx: Option<Gfx>,
    engine: Option<Engine>,
    last: Instant,
    next_frame: Instant,
    error: Option<String>,
}

impl App {
    fn init(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let mut attrs = Window::default_attributes().with_title("Moirai").with_inner_size(LogicalSize::new(1280.0, 800.0));
        if self.opts.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Arc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        if self.opts.fullscreen {
            window.set_cursor_visible(false);
        }
        let instance = instance();
        let surface = instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
        let gpu = Gpu::new(instance, Some(&surface))?;
        let size = window.inner_size();
        let caps = surface.get_capabilities(&gpu.adapter);
        // Prefer a linear 8-bit format: the shader encodes sRGB itself and dithers the result.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm))
            .unwrap_or(caps.formats[0]);
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .ok_or("the window surface is not supported by the GPU")?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 2;
        surface.configure(&gpu.device, &config);
        let renderer = Renderer::new(&gpu, format, config.width, config.height);
        self.gfx = Some(Gfx { window, surface, gpu, config, renderer });
        self.engine = Some(Engine::start(self.cfg));
        self.last = Instant::now();
        Ok(())
    }

    fn draw(&mut self) {
        let (Some(gfx), Some(engine)) = (self.gfx.as_mut(), self.engine.as_mut()) else { return };
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.gpu.device, &gfx.config);
                return;
            }
            // hidden or busy: skip the frame entirely (this is where battery is saved)
            _ => return,
        };
        let now = Instant::now();
        engine.update((now - self.last).as_secs_f64().min(0.25));
        self.last = now;
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let input = engine.frame(self.opts.stars);
        let mut enc = gfx.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        gfx.renderer.render(&gfx.gpu, &mut enc, &view, &input);
        gfx.gpu.queue.submit([enc.finish()]);
        gfx.window.pre_present_notify();
        gfx.gpu.queue.present(frame);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_none()
            && let Err(e) = self.init(el) {
                self.error = Some(e);
                el.exit();
            }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, .. }, .. } => {
                match logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) | Key::Character("q") => el.exit(),
                    Key::Character("n") => {
                        if let Some(e) = self.engine.as_mut() {
                            e.skip();
                        }
                    }
                    Key::Named(NamedKey::Space) => {
                        if let Some(e) = self.engine.as_mut() {
                            e.paused = !e.paused;
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gfx.as_mut() {
                    g.config.width = size.width.max(1);
                    g.config.height = size.height.max(1);
                    g.surface.configure(&g.gpu.device, &g.config);
                    g.renderer.resize(&g.gpu, g.config.width, g.config.height);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_frame {
            if let Some(g) = self.gfx.as_ref() {
                g.window.request_redraw();
            }
            let dt = Duration::from_secs_f64(1.0 / self.opts.fps);
            self.next_frame = (self.next_frame + dt).max(now);
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

pub fn run(cfg: EngineConfig, opts: WindowOptions) -> Result<(), String> {
    let el = EventLoop::new().map_err(|e| e.to_string())?;
    let mut app = App { cfg, opts, gfx: None, engine: None, last: Instant::now(), next_frame: Instant::now(), error: None };
    el.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
