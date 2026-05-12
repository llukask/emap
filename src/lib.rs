//! Slippy-map widget for [egui](https://github.com/emilk/egui).
//!
//! Renders raster tiles using the OSM/XYZ scheme on top of a Web-Mercator
//! projection, with built-in panning, mouse-wheel zoom, and overlay shapes
//! (lines, line strings, circles).
//!
//! The map is built as a builder-style [`Widget`]: configure tile source,
//! initial position, and overlays, then call [`EMap::show`] (or pass the value
//! to `ui.add`).
//!
//! Tiles are fetched through a [`TileLoader`] (defaulting to an async
//! [`TokioTileLoader`] when the `tokio` feature is enabled) and addressed via
//! a [`TileUrlProvider`] (defaulting to [`OsmStandardTileUrlProvider`]).

use std::{collections::HashMap, ops::Deref};

use egui::{
    Color32, Context, CursorIcon, Id, Pos2, Rect, Sense, Stroke, TextureHandle, Vec2, Widget,
};
use egui::{Response, Ui};
use geo::Point;

mod tile_loader;
mod url_provider;

pub use crate::tile_loader::*;
pub use crate::url_provider::*;

/// Per-widget state persisted across frames via `egui`'s temp data store.
///
/// Position is kept in normalized Mercator coordinates (`x, y ∈ [0, 1]`) so it
/// is independent of the chosen tile size and zoom level. `zoom` is stored as
/// `f64` to allow smooth wheel-driven interpolation between integer zoom
/// levels.
#[derive(Clone)]
struct EMapState {
    /// Current zoom level. Fractional values are valid during wheel zoom; the
    /// integer floor selects which tile pyramid level to render.
    zoom: f64,

    /// Map center, x in normalized Mercator space (`0.0` = 180°W, `1.0` = 180°E).
    x: f64,
    /// Map center, y in normalized Mercator space (`0.0` = north pole, `1.0` = south).
    y: f64,

    /// Cache of GPU-uploaded tile textures keyed by tile id. Entries are
    /// evicted by [`EMapState::unload_unused_textures`] once the tile leaves
    /// the viewport, keeping VRAM bounded.
    registered_tile_textures: HashMap<TileId, TextureHandle>,
}

impl EMapState {
    /// Build state centered on a geographic coordinate at a given integer zoom.
    fn with_initial_settings(lat: f64, lon: f64, zoom: u8) -> Self {
        let p = Point::new(lon, lat);
        let coords = normalized_mercator(p);

        let x = coords.x();
        let y = coords.y();

        Self {
            zoom: zoom as f64,
            x,
            y,

            registered_tile_textures: HashMap::new(),
        }
    }

    /// Default state: world centered (`0,0` lat/lon) at zoom 1.
    fn new() -> Self {
        Self {
            zoom: 1.0,
            x: 0.5,
            y: 0.5,

            registered_tile_textures: HashMap::new(),
        }
    }

    /// Load previously stored state for this widget id, if any.
    fn load(ctx: &Context, id: Id) -> Option<Self> {
        ctx.data_mut(|d| d.get_temp(id))
    }

    /// Persist this state for the next frame, keyed by widget id.
    fn store(self, ctx: &Context, id: Id) {
        ctx.data_mut(|d| d.insert_temp(id, self));
    }

    /// Drop cached textures for tiles outside the current viewport.
    ///
    /// Called once per frame after drawing so VRAM usage stays proportional to
    /// the visible area rather than growing with every panned-over tile.
    fn unload_unused_textures(&mut self, visible_tiles: &[TileId]) {
        let set = visible_tiles
            .iter()
            .collect::<std::collections::HashSet<_>>();
        self.registered_tile_textures.retain(|k, _| set.contains(k));
    }
}

/// Overlay primitives the map can draw on top of tiles.
///
/// Coordinates are kept in geographic (lon/lat) form and projected at draw
/// time so overlays stay anchored when the user pans or zooms.
#[derive(Debug, Clone)]
enum Shape {
    /// Single straight segment between two geographic points.
    Line(Point<f64>, Point<f64>, Stroke),
    /// Connected polyline through an arbitrary sequence of geographic points.
    LineString(Vec<Point<f64>>, Stroke),
    /// Circle in screen-space pixels, anchored at a geographic point.
    /// `stroke` and `fill` are independently optional.
    Circle(Point<f64>, f32, Option<Stroke>, Option<Color32>),
}

/// Result of showing the map widget for one frame.
///
/// Dereferences to the underlying egui [`Response`], so callers can use
/// `.clicked()`, `.dragged()`, etc. directly. Adds map-specific data such as
/// the cursor's geographic position.
pub struct EMapResponse {
    /// The egui interaction response for the allocated map rectangle.
    response: Response,

    /// Geographic position (lon/lat) under the mouse cursor, if hovering.
    pointer_position: Option<Point<f64>>,
}

impl EMapResponse {
    /// Geographic position (lon/lat) under the mouse cursor on this frame.
    ///
    /// Returns `None` when the cursor is outside the map rectangle.
    pub fn pointer_position(&self) -> Option<Point<f64>> {
        self.pointer_position
    }
}

impl Deref for EMapResponse {
    type Target = Response;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

/// Builder-style egui widget that renders a slippy map.
///
/// Configure the source (via [`tile_url_provider`](Self::tile_url_provider)
/// and [`tile_loader`](Self::tile_loader)), the initial view (via
/// [`initial_position`](Self::initial_position)), and any overlays
/// ([`line`](Self::line), [`line_string`](Self::line_string),
/// [`filled_circle`](Self::filled_circle), …), then call
/// [`show`](Self::show) or add the widget to a `Ui`.
///
/// The widget keeps persistent state (zoom + center) keyed by the `id` passed
/// to [`new`](Self::new), so multiple maps can coexist within the same UI.
pub struct EMap<'t> {
    /// Stable identity used to look up persisted [`EMapState`] across frames.
    id: egui::Id,
    /// Strategy for turning a [`TileId`] into a fetch URL.
    tile_url_provider: &'t dyn TileUrlProvider,
    /// Optional override for the loader; defaults to [`DEFAULT_TILE_LOADER`].
    tile_loader: Option<&'t dyn TileLoader>,

    /// Tile edge length in screen pixels at integer zoom levels.
    tile_size: f64,

    /// Overlay primitives drawn after the tiles.
    shapes: Vec<Shape>,

    /// Geographic cursor position, computed during [`show`](Self::show) and
    /// exposed back to the caller via [`EMapResponse`].
    pointer_position: Option<Point<f64>>,
}

impl<'t> EMap<'t> {
    /// Create a new map widget.
    ///
    /// `id` is hashed into an [`egui::Id`] and used to persist pan/zoom state
    /// between frames; use distinct ids if you show more than one map.
    ///
    /// Defaults: OpenStreetMap standard tile URLs, [`DEFAULT_TILE_LOADER`],
    /// 256 px tiles, no overlays, world-centered at zoom 1.
    pub fn new(id: impl std::hash::Hash) -> Self {
        Self {
            id: Id::new(id),
            tile_url_provider: &OsmStandardTileUrlProvider,
            tile_loader: None,

            tile_size: 256.0,

            shapes: Vec::new(),

            pointer_position: None,
        }
    }

    /// Seed the map's center and zoom **only on first show**.
    ///
    /// If persisted state already exists for this widget's id, this is a
    /// no-op so user pan/zoom isn't clobbered every frame. Use
    /// [`set_position`](Self::set_position) to force-update an existing view.
    pub fn initial_position(self, ctx: &Context, lat: f64, lon: f64, zoom: u8) -> Self {
        let id = self.id;
        let state = EMapState::with_initial_settings(lat, lon, zoom);
        ctx.data_mut(|d| {
            if d.get_temp::<EMapState>(self.id).is_none() {
                d.insert_temp(id, state);
            }
        });
        self
    }

    /// Set the on-screen edge length of one tile, in pixels.
    ///
    /// The number of tiles fetched per frame scales inversely with this — a
    /// smaller value packs more (sharper) tiles into the same viewport at the
    /// cost of more requests.
    pub fn tile_size(mut self, size: f64) -> Self {
        self.tile_size = size;
        self
    }

    /// Add a straight line overlay between two geographic points.
    pub fn line(mut self, start: Point<f64>, end: Point<f64>, stroke: Stroke) -> Self {
        self.shapes.push(Shape::Line(start, end, stroke));
        self
    }

    /// Add a polyline overlay through a sequence of geographic points.
    pub fn line_string(mut self, points: Vec<Point<f64>>, stroke: Stroke) -> Self {
        self.shapes.push(Shape::LineString(points, stroke));
        self
    }

    /// Override the tile URL strategy (defaults to OpenStreetMap standard).
    pub fn tile_url_provider(mut self, provider: &'t dyn TileUrlProvider) -> Self {
        self.tile_url_provider = provider;
        self
    }

    /// Override the tile loader (defaults to [`DEFAULT_TILE_LOADER`]).
    ///
    /// Supply a [`CachingTileLoader`] to persist tiles on disk between runs.
    pub fn tile_loader(mut self, loader: &'t dyn TileLoader) -> Self {
        self.tile_loader = Some(loader);
        self
    }

    /// Add a circle overlay with independently optional stroke and fill.
    ///
    /// Prefer the convenience constructors [`filled_circle`](Self::filled_circle)
    /// or [`stroke_circle`](Self::stroke_circle) for the common cases.
    pub fn circle(
        mut self,
        center: Point<f64>,
        radius: f32,
        stroke: Option<Stroke>,
        fill: Option<Color32>,
    ) -> Self {
        self.shapes
            .push(Shape::Circle(center, radius, stroke, fill));
        self
    }

    /// Add a solid-filled circle of `radius` pixels at `center` (lon/lat).
    pub fn filled_circle(self, center: Point<f64>, radius: f32, fill: Color32) -> Self {
        self.circle(center, radius, None, Some(fill))
    }

    /// Add an outline-only circle of `radius` pixels at `center` (lon/lat).
    pub fn stroke_circle(self, center: Point<f64>, radius: f32, stroke: Stroke) -> Self {
        self.circle(center, radius, Some(stroke), None)
    }

    /// Discard any persisted pan/zoom state for this widget id.
    ///
    /// The next frame will fall back to the default view (or to
    /// [`initial_position`](Self::initial_position) if also configured).
    pub fn clear_state(self, ctx: &Context) -> Self {
        ctx.data_mut(|d| {
            d.remove::<EMapState>(self.id);
        });
        self
    }

    /// Force-update the persisted center and zoom, overwriting user input.
    ///
    /// Unlike [`initial_position`](Self::initial_position), this applies every
    /// frame it is called — useful for "fly to" commands or external control.
    pub fn set_position(self, ctx: &Context, lat: f64, lon: f64, zoom: u8) -> Self {
        ctx.data_mut(|d| {
            let s = d.get_temp_mut_or_insert_with::<EMapState>(self.id, EMapState::new);

            let p = Point::new(lon, lat);
            let coords = normalized_mercator(p);

            s.x = coords.x();
            s.y = coords.y();
            s.zoom = zoom as f64;
        });
        self
    }

    /// Allocate space, draw the tiles + overlays, and process input.
    ///
    /// The widget claims the [`Ui`]'s available size, so put it inside a panel
    /// or sized container. Returns an [`EMapResponse`] that dereferences to
    /// the underlying egui [`Response`] for click/drag detection and exposes
    /// the cursor's geographic position via
    /// [`pointer_position`](EMapResponse::pointer_position).
    pub fn show(mut self, ui: &mut Ui) -> EMapResponse {
        let mut state = EMapState::load(ui.ctx(), self.id).unwrap_or_else(EMapState::new);

        let dy = ui.input(|r| r.raw_scroll_delta.y);

        let (_id, rect) = ui.allocate_space(ui.available_size());

        let painter = ui.painter_at(rect);

        let w = rect.width() as f64;
        let h = rect.height() as f64;

        let pixel_tile_width = self.tile_size;

        let major = w.max(h);
        let view_rect = view_rect(w, h);

        let desired_tiles = major / pixel_tile_width;

        let n_rect = norm_rect(state.x, state.y, state.zoom, desired_tiles);

        let response = ui
            .interact(rect, self.id, Sense::click_and_drag())
            .on_hover_cursor(CursorIcon::Grab);

        if let Some(pos) = response.hover_pos() {
            if dy.abs() >= 0.01 {
                // Zoom-toward-cursor: capture the geographic point under the
                // cursor *before* changing zoom, then translate the center so
                // that same geographic point ends up under the cursor again
                // after the new zoom is applied. This matches the behaviour
                // of mainstream slippy maps (Google Maps, Leaflet, …).
                let pointer_norm = scale_rect(geo_from_pos2(pos), view_rect, n_rect);

                state.zoom += (dy as f64) * 0.01;
                state.zoom = state.zoom.clamp(0.75, 20.1);

                let n_rect = norm_rect(state.x, state.y, state.zoom, desired_tiles);

                let new_pointer_norm = scale_rect(geo_from_pos2(pos), view_rect, n_rect);

                let desired_diff = pointer_norm - new_pointer_norm;

                state.x += desired_diff.x();
                state.y += desired_diff.y();

                ui.ctx().request_repaint();
            }

            let pointer_norm = scale_rect(geo_from_pos2(pos), view_rect, n_rect);
            let pointer_merc = reverse_normalized_mercator(pointer_norm);

            self.pointer_position = Some(pointer_merc);
        }

        // let a_zoom = ui
        //     .ctx()
        //     .animate_value_with_time("zoom".into(), state.zoom as f32, 0.5);
        // let n_rect = norm_rect(state.x, state.y, state.zoom, desired_tiles);

        ui.ctx().output_mut(|o| o.cursor_icon = CursorIcon::Grab);

        let east = n_rect.min().x;
        let west = n_rect.max().x;
        let north = n_rect.max().y;
        let south = n_rect.min().y;

        let vx_min = view_rect.min().x;
        let vx_max = view_rect.max().x;
        let vy_min = view_rect.min().y;
        let vy_max = view_rect.max().y;

        // Padding of 2 fetches an extra ring of tiles outside the viewport
        // so panning doesn't reveal an unloaded edge while requests are in
        // flight.
        let tiles = TileId::from_bounds(
            reverse_normalized_mercator(Point::new(east as f64, north as f64)),
            reverse_normalized_mercator(Point::new(west as f64, south as f64)),
            state.zoom as u8,
            2,
        );

        for tile in &tiles {
            let top_left = tile.top_left_normalized();
            let bottom_right = tile.bottom_right_normalized();
            let r = Rect::from_min_max(
                Pos2::new(
                    scale(top_left.x(), east, west, vx_min, vx_max) as f32,
                    scale(top_left.y(), south, north, vy_min, vy_max) as f32,
                ),
                Pos2::new(
                    scale(bottom_right.x(), east, west, vx_min, vx_max) as f32,
                    scale(bottom_right.y(), south, north, vy_min, vy_max) as f32,
                ),
            );

            let texture_handle = self.find_texture_handle(tile, &mut state, ui.ctx());

            if let Some((texture_handle, uv)) = texture_handle {
                painter.image(texture_handle.id(), r, uv, Color32::WHITE);
            }
        }
        state.unload_unused_textures(&tiles);

        for shape in &self.shapes {
            match shape {
                Shape::Line(start, end, stroke) => {
                    let line_start = normalized_mercator(*start);
                    let line_end = normalized_mercator(*end);

                    let line_start = Pos2::new(
                        scale(line_start.x(), east, west, vx_min, vx_max) as f32,
                        scale(line_start.y(), south, north, vy_min, vy_max) as f32,
                    );
                    let line_end = Pos2::new(
                        scale(line_end.x(), east, west, vx_min, vx_max) as f32,
                        scale(line_end.y(), south, north, vy_min, vy_max) as f32,
                    );

                    painter.line_segment([line_start, line_end], *stroke);
                }
                Shape::LineString(points, stroke) => {
                    let points = points
                        .iter()
                        .map(|p| {
                            let p = normalized_mercator(*p);
                            Pos2::new(
                                scale(p.x(), east, west, vx_min, vx_max) as f32,
                                scale(p.y(), south, north, vy_min, vy_max) as f32,
                            )
                        })
                        .collect::<Vec<_>>();

                    painter.line(points, *stroke);
                }
                Shape::Circle(point, radius, stroke, fill) => {
                    let center = normalized_mercator(*point);
                    let center = Pos2::new(
                        scale(center.x(), east, west, vx_min, vx_max) as f32,
                        scale(center.y(), south, north, vy_min, vy_max) as f32,
                    );

                    let radius = *radius;

                    if fill.is_some() && stroke.is_some() {
                        let fill_color = fill.unwrap();
                        let stroke = stroke.unwrap();
                        painter.circle(center, radius, fill_color, stroke);
                    } else if fill.is_some() {
                        let fill_color = fill.unwrap();
                        painter.circle_filled(center, radius, fill_color);
                    } else if stroke.is_some() {
                        let stroke = stroke.unwrap();
                        painter.circle_stroke(center, radius, stroke);
                    }
                }
            }
        }

        let drag = response.drag_delta();
        if drag != Vec2::ZERO {
            // input range x 0.0 .. w
            // output range x 0.0 .. (west - east)
            let x = scale(drag.x as f64, 0.0, w, 0.0, west - east);
            // range y 0.0 .. h
            // output range y 0.0 .. (north - south)
            let y = scale(drag.y as f64, 0.0, h, 0.0, north - south);

            state.x -= x;
            state.x = state.x.clamp(0.0, 1.0);

            state.y -= y;
            state.y = state.y.clamp(0.0, 1.0);
        }

        state.store(ui.ctx(), self.id);

        EMapResponse {
            response,
            pointer_position: self.pointer_position,
        }
    }

    /// Resolve the texture (and UV sub-rect) to draw for a given tile.
    ///
    /// Lookup order:
    /// 1. Already-uploaded texture for this exact tile (UV = full image).
    /// 2. Trigger the [`TileLoader`] to fetch the tile; if the loader returns
    ///    data synchronously it's uploaded immediately, otherwise a later
    ///    frame will pick it up.
    /// 3. Pyramid fallback: walk up parent tiles (`z-1`, `z-2`, …) and reuse
    ///    a cached coarser texture, returning the matching quadrant as the
    ///    UV rect. This keeps the map covered with a blurry-but-correct
    ///    image while the higher-zoom tile is still loading.
    fn find_texture_handle(
        &self,
        tile: &TileId,
        state: &mut EMapState,
        ctx: &Context,
    ) -> Option<(TextureHandle, Rect)> {
        let texture_handle = state.registered_tile_textures.get(tile).cloned();
        if let Some(h) = texture_handle {
            let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
            return Some((h, uv));
        }

        let url = self.tile_url_provider.url(*tile).to_string();

        let loader: &dyn TileLoader = self
            .tile_loader
            .unwrap_or_else(|| DEFAULT_TILE_LOADER.deref());

        let img_data = loader.tile(url, tile, ctx.clone());
        if let Some(img_data) = img_data {
            let h = ctx.load_texture(
                format!("{:?}", tile),
                img_data,
                egui::TextureOptions::LINEAR,
            );
            state.registered_tile_textures.insert(*tile, h.clone());
            let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
            return Some((h, uv));
        }

        let (mut new_tile, mut new_uv) =
            tile.zoom_out_with_uv(Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
        loop {
            if new_tile.z == 0 {
                break;
            }

            let texture_handle = state.registered_tile_textures.get(&new_tile).cloned();
            if let Some(h) = texture_handle {
                return Some((h, new_uv));
            }
            (new_tile, new_uv) = new_tile.zoom_out_with_uv(new_uv);
        }

        None
    }
}

impl Widget for EMap<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        self.show(ui).response
    }
}

/// Convert an egui [`Pos2`] (f32 screen pixels) into a `geo::Point<f64>` so
/// it can participate in the projection math without precision loss.
fn geo_from_pos2(p: Pos2) -> Point<f64> {
    Point::new(p.x as f64, p.y as f64)
}

/// Compute the screen-space square that the map renders into.
///
/// The square is centered in the viewport and uses the longer side of the
/// allocated rectangle, so the projection stays isotropic (no horizontal or
/// vertical stretching) regardless of the host UI's aspect ratio.
fn view_rect(w: f64, h: f64) -> geo::Rect<f64> {
    let major = w.max(h);
    let vx_min = (w / 2.0) - (major / 2.0);
    let vx_max = (w / 2.0) + (major / 2.0);
    let vy_min = (h / 2.0) - (major / 2.0);
    let vy_max = (h / 2.0) + (major / 2.0);
    geo::Rect::new(Point::new(vx_min, vy_min), Point::new(vx_max, vy_max))
}

/// Compute the visible rectangle in normalized-Mercator coordinates.
///
/// Given a map center (`x`, `y`), a (possibly fractional) `zoom`, and how
/// many tile-widths the viewport spans (`desired_tiles`), this returns the
/// north/south/east/west bounds in the `[0, 1]` normalized Mercator frame
/// that the renderer projects into screen space.
fn norm_rect(x: f64, y: f64, zoom: f64, desired_tiles: f64) -> geo::Rect<f64> {
    let nf = 2.0f64.powf(zoom);
    let t_side = 1.0 / nf;
    let east = x - (0.5 * desired_tiles * t_side);
    let west = x + (0.5 * desired_tiles * t_side);
    let north = y + (0.5 * desired_tiles * t_side);
    let south = y - (0.5 * desired_tiles * t_side);

    geo::Rect::new(Point::new(east, north), Point::new(west, south))
}

/// Linearly remap `v` from `[src_min, src_max]` into `[tgt_min, tgt_max]`.
///
/// No clamping; values outside the source range extrapolate.
fn scale(v: f64, src_min: f64, src_max: f64, tgt_min: f64, tgt_max: f64) -> f64 {
    let src_range = src_max - src_min;
    let tgt_range = tgt_max - tgt_min;

    let v = (v - src_min) / src_range;
    v * tgt_range + tgt_min
}

/// 2D version of [`scale`]: remap a point from one rectangle into another.
fn scale_rect(pos: Point<f64>, src_space: geo::Rect<f64>, tgt_space: geo::Rect<f64>) -> Point<f64> {
    let x = scale(
        pos.x(),
        src_space.min().x,
        src_space.max().x,
        tgt_space.min().x,
        tgt_space.max().x,
    );
    let y = scale(
        pos.y(),
        src_space.min().y,
        src_space.max().y,
        tgt_space.min().y,
        tgt_space.max().y,
    );
    Point::new(x, y)
}

/// Project a geographic point (lon/lat in degrees) into normalized Web
/// Mercator space, where `(0, 0)` is the top-left of the world tile and
/// `(1, 1)` is the bottom-right.
///
/// The standard Web Mercator y formula `asinh(tan(lat))` is used and then
/// rescaled to the `[0, 1]` range. Latitudes outside ±85.0511° map outside
/// `[0, 1]` (Web Mercator is undefined at the poles).
fn normalized_mercator(p: Point<f64>) -> Point<f64> {
    let lon = p.x();
    let lat = p.y();

    let x_wm = lon;
    let y_wm = lat.to_radians().tan().asinh();

    let x = 0.5 + (x_wm / 360.0);
    let y = 0.5 - (y_wm / (2.0 * std::f64::consts::PI));

    Point::new(x, y)
}

/// Inverse of [`normalized_mercator`]: unproject a normalized Mercator
/// `(x, y) ∈ [0, 1]²` back to geographic (lon, lat) degrees.
fn reverse_normalized_mercator(p: Point<f64>) -> Point<f64> {
    let x = p.x();
    let y = p.y();

    let x_wm = (x - 0.5) * 360.0;
    let y_wm = (0.5 - y) * 2.0 * std::f64::consts::PI;

    let lon = x_wm;
    let lat = y_wm.sinh().atan().to_degrees();

    Point::new(lon, lat)
}

/// Convert a geographic point to fractional tile coordinates at `zoom`.
///
/// The integer part is the XYZ tile id; the fractional part is the position
/// within that tile.
fn tile_coords(p: Point<f64>, zoom: u8) -> Point<f64> {
    let projected = normalized_mercator(p);

    let n = 2.0f64.powi(zoom as i32);

    let x_tile = projected.x() * n;
    let y_tile = projected.y() * n;

    Point::new(x_tile, y_tile)
}

/// Identifier for a single tile in the XYZ slippy-map scheme.
///
/// At zoom `z` the world is divided into a `2^z × 2^z` grid of tiles, where
/// `x` increases east from longitude 180°W and `y` increases south from the
/// Mercator-clamped north edge. This is the same convention as OpenStreetMap,
/// Mapbox, Google Maps, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    /// Column index, `0..2^z`, increasing east.
    pub x: i32,
    /// Row index, `0..2^z`, increasing south.
    pub y: i32,
    /// Zoom level. `0` is one tile covering the whole world.
    pub z: u8,
}

impl TileId {
    /// Tile that contains the given geographic point at the requested zoom.
    fn from_point_and_zoom(p: Point<f64>, zoom: u8) -> Self {
        let coords = tile_coords(p, zoom);
        let x = coords.x() as i32;
        let y = coords.y() as i32;

        Self { x, y, z: zoom }
    }

    /// Enumerate every tile that intersects the geographic rectangle spanned
    /// by `p1` and `p2` at `zoom`, expanded outward by `padding` tiles on
    /// each side.
    ///
    /// Tiles outside the valid `0..2^z` range are skipped, so the result is
    /// safe to feed straight to a tile URL provider without bounds checks.
    fn from_bounds(p1: Point<f64>, p2: Point<f64>, zoom: u8, padding: i32) -> Vec<Self> {
        let left_x = p1.x().min(p2.x());
        let right_x = p1.x().max(p2.x());

        let top_y = p1.y().max(p2.y());
        let bottom_y = p1.y().min(p2.y());

        let top_left = Point::new(left_x, top_y);
        let bottom_right = Point::new(right_x, bottom_y);

        let top_left_tile = Self::from_point_and_zoom(top_left, zoom);
        let bottom_right_tile = Self::from_point_and_zoom(bottom_right, zoom);

        let mut tiles = Vec::new();

        let n = 2u32.pow(zoom as u32);

        for x in (top_left_tile.x - padding)..=(bottom_right_tile.x + padding) {
            for y in (top_left_tile.y - padding)..=(bottom_right_tile.y + padding) {
                if x < 0 || y < 0 || x >= n as i32 || y >= n as i32 {
                    continue;
                }
                tiles.push(Self { x, y, z: zoom });
            }
        }

        tiles
    }

    /// North-west corner of this tile in normalized Mercator space.
    fn top_left_normalized(&self) -> Point<f64> {
        let x = self.x as f64;
        let y = self.y as f64;

        let n = 2.0f64.powi(self.z as i32);

        let x_tile = x / n;
        let y_tile = y / n;

        Point::new(x_tile, y_tile)
    }

    /// South-east corner of this tile in normalized Mercator space.
    fn bottom_right_normalized(&self) -> Point<f64> {
        let x = (self.x + 1) as f64;
        let y = (self.y + 1) as f64;

        let n = 2.0f64.powi(self.z as i32);

        let x_tile = x / n;
        let y_tile = y / n;

        Point::new(x_tile, y_tile)
    }

    /// Step one level up the tile pyramid, returning the parent tile and the
    /// UV sub-rect within that parent corresponding to *this* tile's area.
    ///
    /// Used by the pyramid fallback in [`EMap::find_texture_handle`]: when a
    /// high-zoom tile isn't loaded yet, repeated calls to this walk up the
    /// pyramid until a cached parent is found, and the accumulated UV rect
    /// selects the quadrant that should be drawn in place of the missing
    /// tile.
    fn zoom_out_with_uv(&self, uv: Rect) -> (TileId, Rect) {
        let new_tile = TileId {
            x: self.x / 2,
            y: self.y / 2,
            z: self.z - 1,
        };

        let uv_left = if self.y % 2 == 0 { 0.0 } else { 0.5 };
        let uv_right = uv_left + 0.5;

        let uv_top = if self.x % 2 == 0 { 0.0 } else { 0.5 };
        let uv_bottom = uv_top + 0.5;

        let uv_left = uv.min.y + (uv.height()) * uv_left;
        let uv_right = uv.min.y + (uv.height()) * uv_right;

        let uv_top = uv.min.x + (uv.width()) * uv_top;
        let uv_bottom = uv.min.x + (uv.width()) * uv_bottom;

        let uv = Rect::from_min_max(Pos2::new(uv_top, uv_left), Pos2::new(uv_bottom, uv_right));

        (new_tile, uv)
    }
}
