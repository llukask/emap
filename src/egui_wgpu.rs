//! egui-wgpu integration helpers.
//!
//! Lets a host embed [`crate::EMap`] inside an `egui` UI built on top of
//! `egui_wgpu`'s paint-callback mechanism (eframe's Wgpu backend, or any
//! application that drives `egui_wgpu` directly). Hides the boilerplate
//! around installing the renderer into `callback_resources`, translating
//! egui's logical-pixel input into emap's physical-pixel [`Input`], and
//! routing the [`EMapResponse`] back to the host.
//!
//! Gated by the `egui-wgpu` Cargo feature.
//!
//! # Example
//!
//! ```ignore
//! struct App { emap: emap::egui_wgpu::EmapHandle }
//!
//! impl App {
//!     fn new(cc: &eframe::CreationContext<'_>) -> Self {
//!         let rs = cc.wgpu_render_state.as_ref().unwrap();
//!         let emap = emap::egui_wgpu::EmapHandle::install(rs, &cc.egui_ctx);
//!         emap.with(|e| e.set_initial_position(52.5, 13.4, 8));
//!         Self { emap }
//!     }
//! }
//!
//! impl eframe::App for App {
//!     fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
//!         egui::CentralPanel::default()
//!             .frame(egui::Frame::NONE)
//!             .show(ctx, |ui| {
//!                 let (_rect, _resp) = self.emap.show(ui, Vec::new());
//!             });
//!     }
//! }
//! ```

use std::sync::{Arc, Mutex};

use crate::{EMap, EMapResponse, Frame, Input, RepaintSignal, Shape, Viewport};

/// Handle for an [`EMap`] embedded in an egui-wgpu paint callback.
///
/// Owns the renderer's mutex internally so the egui callback (which only
/// gets shared access to its `callback_resources`) can mutate it; the host
/// holds a clone of the same `Arc` to drive configuration via
/// [`with`](Self::with) and to read [`last_response`](Self::last_response).
pub struct EmapHandle {
    emap: Arc<Mutex<EMap>>,
    last_response: Arc<Mutex<Option<EMapResponse>>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Stashed in `callback_resources` so the [`EmapPaintCallback`] can find
/// the renderer + response slot at paint time.
struct CallbackResource {
    emap: Arc<Mutex<EMap>>,
    last_response: Arc<Mutex<Option<EMapResponse>>>,
}

impl EmapHandle {
    /// Construct an [`EMap`] from the given render state and install it
    /// into the egui-wgpu callback resources.
    ///
    /// The `RepaintSignal` is wired to `ctx.request_repaint()` so async
    /// tile loaders wake the UI on completion.
    pub fn install(rs: &egui_wgpu::RenderState, ctx: &egui::Context) -> Self {
        let ctx2 = ctx.clone();
        let repaint: RepaintSignal = Arc::new(move || ctx2.request_repaint());
        let emap = EMap::new(&rs.device, rs.target_format, repaint);
        Self::install_with(rs, emap)
    }

    /// Install an already-constructed [`EMap`].
    ///
    /// Use this when the caller wants full control over the EMap's
    /// construction (e.g. a custom `RepaintSignal` that batches wakeups
    /// or routes them through a different mechanism than
    /// `Context::request_repaint`).
    pub fn install_with(rs: &egui_wgpu::RenderState, emap: EMap) -> Self {
        let emap = Arc::new(Mutex::new(emap));
        let last_response = Arc::new(Mutex::new(None));
        rs.renderer
            .write()
            .callback_resources
            .insert(CallbackResource {
                emap: emap.clone(),
                last_response: last_response.clone(),
            });
        Self {
            emap,
            last_response,
            device: rs.device.clone(),
            queue: rs.queue.clone(),
        }
    }

    /// Mutate the wrapped [`EMap`]. Acquires the internal mutex; do not
    /// call from within a paint callback (the callback already holds the
    /// lock — would deadlock).
    pub fn with<R>(&self, f: impl FnOnce(&mut EMap) -> R) -> R {
        let mut emap = self.emap.lock().expect("emap mutex poisoned");
        f(&mut emap)
    }

    /// Latest [`EMapResponse`] stashed by the paint callback, if any.
    /// `None` before the first frame is rendered.
    pub fn last_response(&self) -> Option<EMapResponse> {
        self.last_response
            .lock()
            .expect("response mutex poisoned")
            .clone()
    }

    /// Allocate the rest of the available space in `ui` for the map,
    /// gather input (DPI-scaled), and push a paint callback that renders
    /// the tiles + the supplied overlay `shapes`.
    ///
    /// Returns the allocated rect (logical px) and the egui response so
    /// the caller can react to clicks, drags, hover, etc.
    pub fn show(
        &self,
        ui: &mut egui::Ui,
        shapes: Vec<Shape>,
    ) -> (egui::Rect, egui::Response) {
        let avail = ui.available_size();
        let (rect, response) =
            ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

        // egui delivers positions in logical px (already scaled by
        // pixels_per_point); the wgpu surface is sized in physical px.
        // Convert so the viewport, pointer, and drag deltas land in the
        // same space emap reasons in.
        let ppp = ui.ctx().pixels_per_point();

        let viewport = Viewport {
            origin: glam::Vec2::new(rect.min.x * ppp, rect.min.y * ppp),
            size: glam::Vec2::new(rect.width() * ppp, rect.height() * ppp),
        };

        let pointer_position = response.hover_pos().map(|p| {
            glam::Vec2::new((p.x - rect.min.x) * ppp, (p.y - rect.min.y) * ppp)
        });
        let scroll_delta_y = if response.hovered() {
            ui.ctx().input(|i| i.raw_scroll_delta.y)
        } else {
            0.0
        };
        let drag = response.drag_delta();
        let input = Input {
            pointer_position,
            scroll_delta_y,
            drag_delta: glam::Vec2::new(drag.x * ppp, drag.y * ppp),
        };

        let callback = EmapPaintCallback {
            device: self.device.clone(),
            queue: self.queue.clone(),
            viewport,
            input,
            shapes,
        };
        ui.painter()
            .add(egui_wgpu::Callback::new_paint_callback(rect, callback));
        (rect, response)
    }

    /// Project a logical-pixel position *inside the allocated map rect*
    /// into geographic coordinates using the EMap's most-recent state.
    ///
    /// `pixel_in_rect_logical` is `interact_pointer_pos() - rect.min` for
    /// the typical click handler. `rect_size_logical` is the rect's size
    /// in egui's logical px. `pixels_per_point` is whatever
    /// `Context::pixels_per_point` reports.
    pub fn screen_to_geo(
        &self,
        pixel_in_rect_logical: egui::Vec2,
        rect_size_logical: egui::Vec2,
        pixels_per_point: f32,
    ) -> geo::Point<f64> {
        let viewport = Viewport {
            origin: glam::Vec2::ZERO,
            size: glam::Vec2::new(
                rect_size_logical.x * pixels_per_point,
                rect_size_logical.y * pixels_per_point,
            ),
        };
        let pixel = glam::Vec2::new(
            pixel_in_rect_logical.x * pixels_per_point,
            pixel_in_rect_logical.y * pixels_per_point,
        );
        self.emap
            .lock()
            .expect("emap mutex poisoned")
            .screen_to_geo(pixel, viewport)
    }
}

/// Bridges [`egui_wgpu::CallbackTrait`] to [`EMap::render`].
///
/// `device`/`queue` are cheap [`wgpu`] handles (refcounted internally) so
/// cloning them per frame is fine.
struct EmapPaintCallback {
    device: wgpu::Device,
    queue: wgpu::Queue,
    viewport: Viewport,
    input: Input,
    shapes: Vec<Shape>,
}

impl egui_wgpu::CallbackTrait for EmapPaintCallback {
    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let res = resources
            .get::<CallbackResource>()
            .expect("emap CallbackResource missing — call EmapHandle::install first");
        let mut emap = res.emap.lock().expect("emap mutex poisoned");
        let response = emap.render(
            &Frame {
                device: &self.device,
                queue: &self.queue,
                viewport: self.viewport,
                input: self.input,
                shapes: &self.shapes,
            },
            render_pass,
        );
        drop(emap);
        *res.last_response.lock().expect("response mutex poisoned") = Some(response);
    }
}
