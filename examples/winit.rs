//! winit + wgpu demo for the `emap` crate.
//!
//! Wires up a window, a wgpu surface, and an [`emap::EMap`] renderer. Left-
//! click adds a polygon vertex (rendered as a filled translucent overlay),
//! right-click pops the last vertex, scroll-wheel zooms toward the cursor,
//! and dragging with the left button pans. The current center / zoom /
//! pointer geographic position is printed to stdout once per redraw.

use std::sync::Arc;

use emap::{CachingTileLoader, Color, EMap, EMapResponse, Frame, Input, Shape, Stroke, Viewport};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// Sent by the [`emap::RepaintSignal`] when a tile finishes loading.
    Wake,
}

struct GfxState {
    window: Arc<Window>,
    // Surface borrows from the window; the Arc<Window> keeps it alive.
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    emap: EMap,
}

struct App {
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    gfx: Option<GfxState>,

    cursor_pos: Option<glam::Vec2>,
    lmb_down: bool,
    last_drag_pos: Option<glam::Vec2>,
    /// Distance the cursor has moved while LMB has been held down,
    /// reset on each press. Used to distinguish a click from a drag.
    lmb_drag_amount: f32,

    // Inputs accumulated between redraws.
    pending_scroll: f32,
    pending_drag: glam::Vec2,

    polygon_points: Vec<geo::Point<f64>>,
    last_response: Option<EMapResponse>,
}

impl App {
    fn new(proxy: winit::event_loop::EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            gfx: None,
            cursor_pos: None,
            lmb_down: false,
            last_drag_pos: None,
            lmb_drag_amount: 0.0,
            pending_scroll: 0.0,
            pending_drag: glam::Vec2::ZERO,
            polygon_points: Vec::new(),
            last_response: None,
        }
    }

    fn init_gfx(&mut self, el: &ActiveEventLoop) {
        let attrs = Window::default_attributes().with_title("emap — wgpu demo");
        let window = Arc::new(el.create_window(attrs).expect("create_window"));

        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface: wgpu::Surface<'static> = instance
            .create_surface(window.clone())
            .expect("create_surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("request_adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("emap.example.device"),
                required_features: wgpu::Features::empty(),
                required_limits:
                    wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .expect("request_device");

        let caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB format so tile colors render with the expected
        // tonemapping; fall back to whatever the surface supports.
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let proxy = self.proxy.clone();
        let repaint: emap::RepaintSignal = Arc::new(move || {
            let _ = proxy.send_event(UserEvent::Wake);
        });

        let mut emap = EMap::new(&device, surface_format, repaint);
        // Persist tiles between runs so the demo stops hammering OSM.
        // Stored project-local in `cache/<z>/<x>/<y>`.
        emap.set_tile_loader(Arc::new(CachingTileLoader::new("cache")));
        emap.set_initial_position(52.5, 13.4, 8);

        self.gfx = Some(GfxState {
            window,
            surface,
            surface_config,
            device,
            queue,
            emap,
        });
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        let Some(gfx) = &mut self.gfx else { return };
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        gfx.surface_config.width = new_size.width;
        gfx.surface_config.height = new_size.height;
        gfx.surface.configure(&gfx.device, &gfx.surface_config);
        gfx.window.request_redraw();
    }

    fn redraw(&mut self) {
        let Some(gfx) = &mut self.gfx else { return };

        let frame_texture = match gfx.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gfx.surface.configure(&gfx.device, &gfx.surface_config);
                return;
            }
            Err(e) => {
                eprintln!("get_current_texture: {e:?}");
                return;
            }
        };
        let view = frame_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("emap.example.encoder"),
            });

        let viewport = Viewport {
            origin: glam::Vec2::ZERO,
            size: glam::Vec2::new(
                gfx.surface_config.width as f32,
                gfx.surface_config.height as f32,
            ),
        };

        let input = Input {
            pointer_position: self.cursor_pos,
            scroll_delta_y: std::mem::take(&mut self.pending_scroll),
            drag_delta: std::mem::take(&mut self.pending_drag),
        };

        let shapes: Vec<Shape> = if self.polygon_points.len() >= 3 {
            vec![Shape::polygon(
                self.polygon_points.clone(),
                Some(Stroke::new(2.0, Color::rgba(255, 255, 255, 255))),
                Some(Color::rgba(220, 40, 40, 96)),
            )]
        } else if self.polygon_points.len() == 2 {
            vec![Shape::line(
                self.polygon_points[0],
                self.polygon_points[1],
                Stroke::new(2.0, Color::WHITE),
            )]
        } else {
            Vec::new()
        };

        let response = {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("emap.example.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.06,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            gfx.emap.render(
                &Frame {
                    device: &gfx.device,
                    queue: &gfx.queue,
                    viewport,
                    input,
                    shapes: &shapes,
                },
                &mut pass,
            )
        };

        gfx.queue.submit(Some(encoder.finish()));
        frame_texture.present();

        if let Some(p) = response.pointer_position() {
            print!(
                "\rcenter {:.4},{:.4}  zoom {:.2}  cursor {:.4},{:.4}    ",
                response.center().y(),
                response.center().x(),
                response.zoom(),
                p.y(),
                p.x(),
            );
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();

        self.last_response = Some(response);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_none() {
            self.init_gfx(el);
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _event: UserEvent) {
        if let Some(gfx) = &self.gfx {
            gfx.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => self.resize(size),
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(gfx) = &self.gfx {
                    let s = gfx.window.inner_size();
                    self.resize(s);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let new_pos = glam::Vec2::new(position.x as f32, position.y as f32);
                if self.lmb_down {
                    if let Some(prev) = self.last_drag_pos {
                        let d = new_pos - prev;
                        self.pending_drag += d;
                        self.lmb_drag_amount += d.length();
                    }
                    self.last_drag_pos = Some(new_pos);
                }
                self.cursor_pos = Some(new_pos);
                if let Some(gfx) = &self.gfx {
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = None;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.pending_scroll += match delta {
                    // PixelDelta units already match the egui convention
                    // closely enough for the zoom-step constant.
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                    MouseScrollDelta::LineDelta(_, y) => y * 50.0,
                };
                if let Some(gfx) = &self.gfx {
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    self.lmb_down = true;
                    self.last_drag_pos = self.cursor_pos;
                    self.lmb_drag_amount = 0.0;
                }
                (MouseButton::Left, ElementState::Released) => {
                    // Treat very-low-movement release as a click and
                    // push the cursor's geographic position from the
                    // last response onto the polygon ring.
                    if self.lmb_drag_amount < 3.0
                        && let Some(geo_p) = self
                            .last_response
                            .as_ref()
                            .and_then(|r| r.pointer_position())
                    {
                        self.polygon_points.push(geo_p);
                        if let Some(gfx) = &self.gfx {
                            gfx.window.request_redraw();
                        }
                    }
                    self.lmb_down = false;
                    self.last_drag_pos = None;
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    self.polygon_points.pop();
                    if let Some(gfx) = &self.gfx {
                        gfx.window.request_redraw();
                    }
                }
                _ => {}
            },
            WindowEvent::RedrawRequested => {
                self.redraw();
            }
            _ => {}
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,emap=info")),
        )
        .init();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("EventLoop::build");
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).expect("run_app");
}
