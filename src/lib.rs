//! Slippy-map renderer built on [`wgpu`].
//!
//! Renders raster tiles using the OSM/XYZ scheme on top of a Web-Mercator
//! projection, with overlay primitives (lines, line strings, circles, and
//! polygons). Input handling, pan, and wheel-zoom logic live in the
//! library; the renderer itself is "bring your own surface" — the caller
//! supplies an active `wgpu::RenderPass` for the surface texture each
//! frame.
//!
//! Tiles are fetched through a [`TileLoader`] (defaulting to an async
//! [`TokioTileLoader`] when the `tokio` feature is enabled) and addressed
//! via a [`TileUrlProvider`] (defaulting to [`OsmStandardTileUrlProvider`]).
//!
//! # Quick start
//!
//! ```ignore
//! let mut emap = EMap::new(
//!     &device,
//!     surface_format,
//!     Arc::new(move || window.request_redraw()),
//! );
//! emap.set_initial_position(52.5, 13.4, 8);
//!
//! // Each frame:
//! let response = emap.render(
//!     &Frame {
//!         device: &device,
//!         queue: &queue,
//!         viewport: Viewport { origin: glam::Vec2::ZERO, size: window_size },
//!         input: Input { pointer_position: Some(mouse_xy), scroll_delta_y: dy, drag_delta },
//!         shapes: &[],
//!     },
//!     &mut render_pass,
//! );
//! ```

use std::sync::Arc;

use geo::Point;

mod coords;
mod overlays;
mod tiles;

mod tile_loader;
mod url_provider;

#[cfg(feature = "egui-wgpu")]
pub mod egui_wgpu;

pub use coords::TileId;
pub use tile_loader::*;
pub use url_provider::*;

use coords::{
    norm_rect, normalized_mercator, reverse_normalized_mercator, scale, scale_rect, view_rect,
    UvRect,
};
use overlays::{OverlayRenderer, OverlayShape};
use tiles::{TileDraw, TileRenderer};

// ─── Public types ──────────────────────────────────────────────────────────

/// Straight (non-premultiplied) sRGB color with 8-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    pub const GREEN: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// Outline style: width (in viewport pixels) and color.
#[derive(Debug, Clone, Copy)]
pub struct Stroke {
    pub width: f32,
    pub color: Color,
}

impl Stroke {
    pub const NONE: Stroke = Stroke {
        width: 0.0,
        color: Color::TRANSPARENT,
    };
    pub const fn new(width: f32, color: Color) -> Self {
        Self { width, color }
    }
}

/// Pixel rectangle the renderer should paint into.
///
/// `origin` is the top-left corner in surface pixels and `size` the width
/// and height. The renderer issues `set_viewport` on the supplied
/// `RenderPass`, so everything inside the library reasons in
/// viewport-local pixels.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub origin: glam::Vec2,
    pub size: glam::Vec2,
}

/// Per-frame input state.
///
/// `pointer_position` is the cursor location in viewport-local pixels (top-
/// left origin) or `None` when the cursor is outside or absent.
/// `scroll_delta_y` is the raw wheel delta in egui-style units (positive ⇒
/// zoom in). `drag_delta` is the cursor delta accumulated *while a drag
/// button is held this frame*; pass `Vec2::ZERO` when no drag is in
/// progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct Input {
    pub pointer_position: Option<glam::Vec2>,
    pub scroll_delta_y: f32,
    pub drag_delta: glam::Vec2,
}

/// Bundle of inputs the caller passes to [`EMap::render`].
pub struct Frame<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub viewport: Viewport,
    pub input: Input,
    pub shapes: &'a [Shape],
}

/// Result of rendering one frame.
#[derive(Debug, Clone)]
pub struct EMapResponse {
    pointer_position: Option<Point<f64>>,
    visible_bounds: geo::Rect<f64>,
    projected_bounds: geo::Rect<f64>,
    center: Point<f64>,
    zoom: f64,
}

impl EMapResponse {
    /// Geographic position (lon/lat) under the cursor this frame, or `None`
    /// when the cursor is outside the map.
    pub fn pointer_position(&self) -> Option<Point<f64>> {
        self.pointer_position
    }
    /// Geographic bounds (lon/lat) of exactly what's on screen.
    pub fn visible_bounds(&self) -> geo::Rect<f64> {
        self.visible_bounds
    }
    /// Geographic bounds of the centered projection square (a superset of
    /// [`visible_bounds`](Self::visible_bounds) when the viewport is
    /// non-square).
    pub fn projected_bounds(&self) -> geo::Rect<f64> {
        self.projected_bounds
    }
    /// Map center (lon/lat) at the end of this frame.
    pub fn center(&self) -> Point<f64> {
        self.center
    }
    /// Current zoom level (fractional during interactive wheel zoom).
    pub fn zoom(&self) -> f64 {
        self.zoom
    }
}

/// Overlay primitives the map can draw on top of tiles. All coordinates are
/// geographic and projected to screen at draw time.
#[derive(Debug, Clone)]
pub enum Shape {
    Line(Point<f64>, Point<f64>, Stroke),
    LineString(Vec<Point<f64>>, Stroke),
    Circle(Point<f64>, f32, Option<Stroke>, Option<Color>),
    Polygon(Vec<Point<f64>>, Option<Stroke>, Option<Color>),
}

impl Shape {
    pub fn line(start: Point<f64>, end: Point<f64>, stroke: Stroke) -> Self {
        Shape::Line(start, end, stroke)
    }
    pub fn line_string(points: Vec<Point<f64>>, stroke: Stroke) -> Self {
        Shape::LineString(points, stroke)
    }
    pub fn circle(
        center: Point<f64>,
        radius: f32,
        stroke: Option<Stroke>,
        fill: Option<Color>,
    ) -> Self {
        Shape::Circle(center, radius, stroke, fill)
    }
    pub fn filled_circle(center: Point<f64>, radius: f32, fill: Color) -> Self {
        Shape::Circle(center, radius, None, Some(fill))
    }
    pub fn stroke_circle(center: Point<f64>, radius: f32, stroke: Stroke) -> Self {
        Shape::Circle(center, radius, Some(stroke), None)
    }
    /// Polygon with independently optional stroke and fill. Unlike the
    /// previous egui-based implementation, lyon fills any simple polygon
    /// (convex or not) correctly.
    pub fn polygon(
        points: Vec<Point<f64>>,
        stroke: Option<Stroke>,
        fill: Option<Color>,
    ) -> Self {
        Shape::Polygon(points, stroke, fill)
    }
    pub fn filled_polygon(points: Vec<Point<f64>>, fill: Color) -> Self {
        Shape::Polygon(points, None, Some(fill))
    }
    pub fn stroke_polygon(points: Vec<Point<f64>>, stroke: Stroke) -> Self {
        Shape::Polygon(points, Some(stroke), None)
    }
}

// ─── Internal state ────────────────────────────────────────────────────────

/// Pan + zoom state. Kept in normalized Mercator space (`x, y ∈ [0, 1]`).
#[derive(Debug, Clone)]
struct EMapState {
    zoom: f64,
    x: f64,
    y: f64,
}

impl EMapState {
    fn new() -> Self {
        Self {
            zoom: 1.0,
            x: 0.5,
            y: 0.5,
        }
    }

    fn with_position(lat: f64, lon: f64, zoom: u8) -> Self {
        let coords = normalized_mercator(Point::new(lon, lat));
        Self {
            zoom: zoom as f64,
            x: coords.x(),
            y: coords.y(),
        }
    }
}

// ─── EMap ──────────────────────────────────────────────────────────────────

/// Slippy-map renderer.
///
/// Owns the wgpu pipelines, the GPU tile-texture cache, and the persistent
/// pan/zoom state. Call [`EMap::render`] once per frame inside an active
/// render pass for the surface texture.
pub struct EMap {
    state: EMapState,
    /// Set once the user has positioned the map (via either
    /// [`set_initial_position`](Self::set_initial_position) or
    /// [`set_position`](Self::set_position)). Distinguishes "default view
    /// because user hasn't said anything yet" from "user explicitly chose
    /// 0,0 zoom 1".
    positioned: bool,

    tile_url_provider: Arc<dyn TileUrlProvider>,
    tile_loader: Arc<dyn TileLoader>,
    tile_size: f64,
    /// Upper bound applied to the float zoom value. Defaults to 19.0
    /// (OSM's deepest tile level). Override via [`set_max_zoom`](Self::set_max_zoom).
    max_zoom: f64,

    tile_renderer: TileRenderer,
    overlay_renderer: OverlayRenderer,

    /// Capture given to async loaders so they can wake the host when a
    /// tile finishes loading.
    repaint: RepaintSignal,

    /// Wall-clock reference for animating the loading indicator.
    start: std::time::Instant,
    /// Whether to draw a pulsing dot over tiles being fetched.
    show_loading_indicator: bool,
}

impl EMap {
    /// Build a renderer.
    ///
    /// `target_format` must match the texture format of the render pass
    /// the caller will later supply. `repaint` is invoked by async tile
    /// loaders when a tile is ready — wire it to the host's redraw
    /// mechanism (e.g. `window.request_redraw()` on winit).
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        repaint: RepaintSignal,
    ) -> Self {
        Self {
            state: EMapState::new(),
            positioned: false,
            tile_url_provider: Arc::new(OsmStandardTileUrlProvider),
            tile_loader: DEFAULT_TILE_LOADER.clone(),
            tile_size: 256.0,
            max_zoom: 19.0,
            tile_renderer: TileRenderer::new(device, target_format),
            overlay_renderer: OverlayRenderer::new(device, target_format),
            repaint,
            start: std::time::Instant::now(),
            show_loading_indicator: true,
        }
    }

    /// Enable or disable the pulsing dot drawn over tiles being loaded.
    pub fn set_loading_indicator(&mut self, enabled: bool) {
        self.show_loading_indicator = enabled;
    }

    /// Override the URL provider (default: OpenStreetMap).
    pub fn set_tile_url_provider(&mut self, provider: Arc<dyn TileUrlProvider>) {
        self.tile_url_provider = provider;
    }

    /// Override the tile loader (default: [`DEFAULT_TILE_LOADER`]).
    pub fn set_tile_loader(&mut self, loader: Arc<dyn TileLoader>) {
        self.tile_loader = loader;
    }

    /// Set the on-screen edge length of one tile in viewport pixels.
    pub fn set_tile_size(&mut self, size: f64) {
        self.tile_size = size;
    }

    /// Set the upper bound on the float zoom value.
    ///
    /// Applies both to wheel-driven zoom and to explicit
    /// [`set_position`](Self::set_position) /
    /// [`set_initial_position`](Self::set_initial_position) calls.
    /// Default: `19.0` (deepest tile level served by OSM).
    pub fn set_max_zoom(&mut self, max_zoom: f64) {
        self.max_zoom = max_zoom;
        // Re-clamp current state so an immediate lowering takes effect
        // without waiting for the next scroll event.
        if self.state.zoom > self.max_zoom {
            self.state.zoom = self.max_zoom;
        }
    }

    /// Seed center + zoom only on first show.
    ///
    /// No-op once the user has interacted or [`set_position`](Self::set_position)
    /// has been called.
    pub fn set_initial_position(&mut self, lat: f64, lon: f64, zoom: u8) {
        if !self.positioned {
            self.state = EMapState::with_position(lat, lon, zoom);
            if self.state.zoom > self.max_zoom {
                self.state.zoom = self.max_zoom;
            }
            self.positioned = true;
        }
    }

    /// Force-update the persisted center and zoom.
    pub fn set_position(&mut self, lat: f64, lon: f64, zoom: u8) {
        self.state = EMapState::with_position(lat, lon, zoom);
        if self.state.zoom > self.max_zoom {
            self.state.zoom = self.max_zoom;
        }
        self.positioned = true;
    }

    /// Clear pan/zoom state (returns to default world-centered view).
    pub fn clear_state(&mut self) {
        self.state = EMapState::new();
        self.positioned = false;
    }

    /// Project a viewport-local pixel position to geographic (lon/lat).
    ///
    /// Reflects the most-recently-rendered state — i.e. the answer matches
    /// what the cursor pointed at on the previous frame. Useful for click
    /// handlers that need to convert mouse coordinates before the next
    /// [`render`](Self::render) call.
    pub fn screen_to_geo(&self, pixel: glam::Vec2, viewport: Viewport) -> Point<f64> {
        let w = viewport.size.x as f64;
        let h = viewport.size.y as f64;
        let major = w.max(h);
        let desired_tiles = major / self.tile_size;
        let view = view_rect(w, h);
        let n_rect = norm_rect(self.state.x, self.state.y, self.state.zoom, desired_tiles);
        let pos = Point::new(pixel.x as f64, pixel.y as f64);
        let pointer_norm = scale_rect(pos, view, n_rect);
        reverse_normalized_mercator(pointer_norm)
    }

    /// Render one frame.
    pub fn render(
        &mut self,
        frame: &Frame<'_>,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> EMapResponse {
        let Frame {
            device,
            queue,
            viewport,
            input,
            shapes,
        } = *frame;

        // Restrict draws (and clears) to the requested viewport rect.
        pass.set_viewport(
            viewport.origin.x,
            viewport.origin.y,
            viewport.size.x,
            viewport.size.y,
            0.0,
            1.0,
        );

        let w = viewport.size.x as f64;
        let h = viewport.size.y as f64;
        let major = w.max(h);
        let pixel_tile_width = self.tile_size;
        let desired_tiles = major / pixel_tile_width;
        let view = view_rect(w, h);
        let n_rect = norm_rect(self.state.x, self.state.y, self.state.zoom, desired_tiles);

        // ── Apply wheel zoom ───────────────────────────────────────────
        if let Some(pos) = input.pointer_position
            && input.scroll_delta_y.abs() >= 0.01
        {
            // Zoom-toward-cursor: keep the geographic point under the
            // cursor anchored across the zoom change.
            let pos_geo = Point::new(pos.x as f64, pos.y as f64);
            let pointer_norm = scale_rect(pos_geo, view, n_rect);

            self.state.zoom += input.scroll_delta_y as f64 * 0.01;
            self.state.zoom = self.state.zoom.clamp(0.75, self.max_zoom);

            let n_rect2 =
                norm_rect(self.state.x, self.state.y, self.state.zoom, desired_tiles);
            let new_pointer_norm = scale_rect(pos_geo, view, n_rect2);
            let diff = pointer_norm - new_pointer_norm;
            self.state.x += diff.x();
            self.state.y += diff.y();

            (self.repaint)();
        }

        // Recompute after potential zoom change.
        let n_rect = norm_rect(self.state.x, self.state.y, self.state.zoom, desired_tiles);

        // Pointer in geographic space for the response.
        let pointer_position = input.pointer_position.map(|pos| {
            let pos_geo = Point::new(pos.x as f64, pos.y as f64);
            let pointer_norm = scale_rect(pos_geo, view, n_rect);
            reverse_normalized_mercator(pointer_norm)
        });

        let east = n_rect.min().x;
        let west = n_rect.max().x;
        let north = n_rect.max().y;
        let south = n_rect.min().y;

        let vx_min = view.min().x;
        let vx_max = view.max().x;
        let vy_min = view.min().y;
        let vy_max = view.max().y;

        let current_z = self.state.zoom as u8;
        let bounds_tl = reverse_normalized_mercator(Point::new(east, north));
        let bounds_br = reverse_normalized_mercator(Point::new(west, south));
        // Padding=2 fetches an extra ring of tiles outside the viewport so
        // panning doesn't reveal an unloaded edge while requests are in flight.
        let tiles = TileId::from_bounds(bounds_tl, bounds_br, current_z, 2);

        // Cross-zoom preload — warm both the loader and the GPU cache so
        // fractional-zoom crossings find adjacent-level textures resident.
        let preload_parent = if current_z > 0 {
            TileId::from_bounds(bounds_tl, bounds_br, current_z - 1, 2)
        } else {
            Vec::new()
        };
        let preload_child = if current_z < u8::MAX {
            TileId::from_bounds(bounds_tl, bounds_br, current_z + 1, 2)
        } else {
            Vec::new()
        };

        // ── Build the visible tile draw list ──────────────────────────
        let mut draws: Vec<TileDraw> = Vec::with_capacity(tiles.len());
        let mut loading_rects: Vec<(glam::Vec2, glam::Vec2)> = Vec::new();
        for tile in &tiles {
            let tl = tile.top_left_normalized();
            let br = tile.bottom_right_normalized();
            let rect_min = glam::Vec2::new(
                scale(tl.x(), east, west, vx_min, vx_max) as f32,
                scale(tl.y(), south, north, vy_min, vy_max) as f32,
            );
            let rect_max = glam::Vec2::new(
                scale(br.x(), east, west, vx_min, vx_max) as f32,
                scale(br.y(), south, north, vy_min, vy_max) as f32,
            );

            if let Some(draw) = self.resolve_tile(
                *tile,
                rect_min,
                rect_max,
                device,
                queue,
                &mut loading_rects,
            ) {
                draws.push(draw);
            }
        }

        // Warm preload tiles — fire the loader but don't paint anything.
        for tile in preload_parent.iter().chain(preload_child.iter()) {
            self.warm_tile(*tile, device, queue);
        }

        // Evict GPU textures outside the union of visible + preload sets.
        self.tile_renderer.retain(
            tiles
                .iter()
                .chain(preload_parent.iter())
                .chain(preload_child.iter()),
        );

        // ── Project overlay shapes to viewport-local pixel space ─────
        let project = |p: Point<f64>| -> glam::Vec2 {
            let norm = normalized_mercator(p);
            glam::Vec2::new(
                scale(norm.x(), east, west, vx_min, vx_max) as f32,
                scale(norm.y(), south, north, vy_min, vy_max) as f32,
            )
        };

        // Borrow-checker: keep per-shape projected point Vecs alive while
        // OverlayShape borrows them.
        let mut polyline_points: Vec<Vec<glam::Vec2>> = Vec::new();
        let mut polygon_points: Vec<Vec<glam::Vec2>> = Vec::new();
        for shape in shapes {
            match shape {
                Shape::Line(_, _, _) => {}
                Shape::LineString(pts, _) => {
                    polyline_points.push(pts.iter().copied().map(project).collect());
                }
                Shape::Circle(_, _, _, _) => {}
                Shape::Polygon(pts, _, _) => {
                    polygon_points.push(pts.iter().copied().map(project).collect());
                }
            }
        }
        // Single-segment Lines: stash a length-2 vec alongside.
        let mut line_points: Vec<Vec<glam::Vec2>> = Vec::new();
        for shape in shapes {
            if let Shape::Line(a, b, _) = shape {
                line_points.push(vec![project(*a), project(*b)]);
            }
        }

        let mut overlay_shapes: Vec<OverlayShape<'_>> = Vec::new();

        // Loading indicators are drawn first so user shapes paint on top.
        if self.show_loading_indicator && !loading_rects.is_empty() {
            let t = self.start.elapsed().as_secs_f32();
            // Sinusoidal pulse, 3 rad/s ≈ ~0.5 Hz visible cycle.
            let pulse = 0.5 + 0.5 * (t * 3.0).sin();
            for (min, max) in &loading_rects {
                let center = (*min + *max) * 0.5;
                let base = (max.x - min.x).min(max.y - min.y) * 0.08;
                // Static white ring …
                overlay_shapes.push(OverlayShape::CircleStroke {
                    center,
                    radius: base,
                    stroke: Stroke::new(2.0, Color::rgba(255, 255, 255, 180)),
                });
                // … with pulsing translucent fill inside.
                let inner = base * (0.35 + 0.5 * pulse);
                let alpha = (60.0 + 160.0 * pulse) as u8;
                overlay_shapes.push(OverlayShape::CircleFill {
                    center,
                    radius: inner,
                    fill: Color::rgba(255, 255, 255, alpha),
                });
            }
        }

        let mut ls_i = 0;
        let mut pg_i = 0;
        let mut ln_i = 0;
        for shape in shapes {
            match shape {
                Shape::Line(_, _, stroke) => {
                    overlay_shapes.push(OverlayShape::Polyline {
                        points: &line_points[ln_i],
                        stroke: *stroke,
                        closed: false,
                    });
                    ln_i += 1;
                }
                Shape::LineString(_, stroke) => {
                    overlay_shapes.push(OverlayShape::Polyline {
                        points: &polyline_points[ls_i],
                        stroke: *stroke,
                        closed: false,
                    });
                    ls_i += 1;
                }
                Shape::Circle(center, radius, stroke, fill) => {
                    let c = project(*center);
                    if let Some(fill) = fill {
                        overlay_shapes.push(OverlayShape::CircleFill {
                            center: c,
                            radius: *radius,
                            fill: *fill,
                        });
                    }
                    if let Some(stroke) = stroke {
                        overlay_shapes.push(OverlayShape::CircleStroke {
                            center: c,
                            radius: *radius,
                            stroke: *stroke,
                        });
                    }
                }
                Shape::Polygon(_, stroke, fill) => {
                    if let Some(fill) = fill {
                        overlay_shapes.push(OverlayShape::PolygonFill {
                            points: &polygon_points[pg_i],
                            fill: *fill,
                        });
                    }
                    if let Some(stroke) = stroke {
                        overlay_shapes.push(OverlayShape::Polyline {
                            points: &polygon_points[pg_i],
                            stroke: *stroke,
                            closed: true,
                        });
                    }
                    pg_i += 1;
                }
            }
        }

        // ── Issue draws ───────────────────────────────────────────────
        self.tile_renderer
            .render(device, queue, viewport.size, &draws, pass);
        self.overlay_renderer
            .render(device, queue, viewport.size, &overlay_shapes, pass);

        // Keep redrawing while tiles are loading so the indicator animates.
        if self.show_loading_indicator && !loading_rects.is_empty() {
            (self.repaint)();
        }

        // ── Apply drag delta after rendering (matches original ordering)
        if input.drag_delta != glam::Vec2::ZERO {
            let dx = scale(input.drag_delta.x as f64, 0.0, w, 0.0, west - east);
            let dy = scale(input.drag_delta.y as f64, 0.0, h, 0.0, north - south);
            self.state.x -= dx;
            self.state.x = self.state.x.clamp(0.0, 1.0);
            self.state.y -= dy;
            self.state.y = self.state.y.clamp(0.0, 1.0);
            self.positioned = true;
        }

        // ── Build response with post-drag bounds ─────────────────────
        let n_rect_after =
            norm_rect(self.state.x, self.state.y, self.state.zoom, desired_tiles);

        let visible_tl_norm = scale_rect(
            Point::new(0.0, 0.0),
            view,
            n_rect_after,
        );
        let visible_br_norm = scale_rect(
            Point::new(w, h),
            view,
            n_rect_after,
        );
        let visible_bounds = geo::Rect::new(
            reverse_normalized_mercator(visible_tl_norm),
            reverse_normalized_mercator(visible_br_norm),
        );
        let projected_bounds = geo::Rect::new(
            reverse_normalized_mercator(Point::from(n_rect_after.min())),
            reverse_normalized_mercator(Point::from(n_rect_after.max())),
        );

        EMapResponse {
            pointer_position,
            visible_bounds,
            projected_bounds,
            center: reverse_normalized_mercator(Point::new(self.state.x, self.state.y)),
            zoom: self.state.zoom,
        }
    }

    /// Resolve a draw for `tile`, uploading newly-arrived tile images and
    /// falling back to a cached pyramid parent if the exact tile isn't
    /// resident yet. Pushes the tile's screen rect onto `loading_rects` when
    /// the loader reports the tile is still in flight.
    fn resolve_tile(
        &mut self,
        tile: TileId,
        rect_min: glam::Vec2,
        rect_max: glam::Vec2,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        loading_rects: &mut Vec<(glam::Vec2, glam::Vec2)>,
    ) -> Option<TileDraw> {
        if self.tile_renderer.contains(&tile) {
            return Some(TileDraw::full_uv(tile, rect_min, rect_max));
        }

        let url = self.tile_url_provider.url(tile);
        match self.tile_loader.tile(url, &tile, self.repaint.clone()) {
            TileFetch::Ready(img) => {
                self.tile_renderer.upload(device, queue, tile, &img);
                return Some(TileDraw::full_uv(tile, rect_min, rect_max));
            }
            TileFetch::Loading => {
                loading_rects.push((rect_min, rect_max));
            }
        }

        // Pyramid fallback: walk up parents looking for a cached coarser
        // texture, sampling the matching sub-rect.
        let (mut parent, mut uv) = tile.zoom_out_with_uv(UvRect::FULL);
        loop {
            if self.tile_renderer.contains(&parent) {
                return Some(TileDraw::with_uv(parent, rect_min, rect_max, uv));
            }
            if parent.z == 0 {
                return None;
            }
            (parent, uv) = parent.zoom_out_with_uv(uv);
        }
    }

    /// Issue a fetch for a preload tile without taking a screen rect. The
    /// returned image (if any) is uploaded so the next frame's
    /// [`resolve_tile`](Self::resolve_tile) call finds it in the cache.
    fn warm_tile(&mut self, tile: TileId, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.tile_renderer.contains(&tile) {
            return;
        }
        let url = self.tile_url_provider.url(tile);
        if let TileFetch::Ready(img) = self.tile_loader.tile(url, &tile, self.repaint.clone()) {
            self.tile_renderer.upload(device, queue, tile, &img);
        }
    }
}
