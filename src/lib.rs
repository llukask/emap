use std::{collections::HashMap, ops::Deref};

use egui::{
    Color32, Context, CursorIcon, Id, Pos2, Rect, Sense, Stroke, TextureHandle, Vec2, Widget,
};
use geo::Point;

mod tile_loader;
mod url_provider;

pub use crate::tile_loader::*;
pub use crate::url_provider::*;

#[derive(Clone)]
struct EMapState {
    zoom: f64,

    x: f64,
    y: f64,

    registered_tile_textures: HashMap<TileId, TextureHandle>,
}

impl EMapState {
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

    fn new() -> Self {
        Self {
            zoom: 1.0,
            x: 0.5,
            y: 0.5,

            registered_tile_textures: HashMap::new(),
        }
    }

    fn load(ctx: &Context, id: Id) -> Option<Self> {
        ctx.data_mut(|d| d.get_temp(id))
    }

    fn store(self, ctx: &Context, id: Id) {
        ctx.data_mut(|d| d.insert_temp(id, self));
    }
}

#[derive(Debug, Clone)]
enum Shape {
    Line(Point<f64>, Point<f64>, Stroke),
    LineString(Vec<Point<f64>>, Stroke),
    Circle(Point<f64>, f32, Option<Stroke>, Option<Color32>),
}

pub struct EMap<'t> {
    id: egui::Id,
    tile_url_provider: &'t dyn TileUrlProvider,
    tile_loader: Option<&'t dyn TileLoader>,

    tile_size: f64,

    shapes: Vec<Shape>,
}

impl<'t> EMap<'t> {
    pub fn new(id: impl std::hash::Hash) -> Self {
        Self {
            id: Id::new(id),
            tile_url_provider: &OsmStandardTileUrlProvider,
            tile_loader: None,

            tile_size: 256.0,

            shapes: Vec::new(),
        }
    }

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

    pub fn tile_size(mut self, size: f64) -> Self {
        self.tile_size = size;
        self
    }

    pub fn line(mut self, start: Point<f64>, end: Point<f64>, stroke: Stroke) -> Self {
        self.shapes.push(Shape::Line(start, end, stroke));
        self
    }

    pub fn line_string(mut self, points: Vec<Point<f64>>, stroke: Stroke) -> Self {
        self.shapes.push(Shape::LineString(points, stroke));
        self
    }

    pub fn tile_url_provider(mut self, provider: &'t dyn TileUrlProvider) -> Self {
        self.tile_url_provider = provider;
        self
    }

    pub fn tile_loader(mut self, loader: &'t dyn TileLoader) -> Self {
        self.tile_loader = Some(loader);
        self
    }

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

    pub fn filled_circle(self, center: Point<f64>, radius: f32, fill: Color32) -> Self {
        self.circle(center, radius, None, Some(fill))
    }

    pub fn stroke_circle(self, center: Point<f64>, radius: f32, stroke: Stroke) -> Self {
        self.circle(center, radius, Some(stroke), None)
    }

    pub fn clear_state(self, ctx: &Context) -> Self {
        ctx.data_mut(|d| {
            d.remove::<EMapState>(self.id);
        });
        self
    }

    pub fn set_position(self, ctx: &Context, lat: f64, lon: f64, zoom: u8) -> Self {
        ctx.data_mut(|d| {
            let s = d.get_temp_mut_or_insert_with::<EMapState>(self.id, || EMapState::new());

            let p = Point::new(lon, lat);
            let coords = normalized_mercator(p);

            s.x = coords.x();
            s.y = coords.y();
            s.zoom = zoom as f64;
        });
        self
    }

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

        let r = ui
            .interact(rect, self.id, Sense::drag().union(Sense::hover()))
            .on_hover_cursor(CursorIcon::Grab);

        if let Some(pos) = r.hover_pos() {
            if dy.abs() >= 0.01 {
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

        let drag = r.drag_delta();
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

        ui.response()
    }
}

fn geo_from_pos2(p: Pos2) -> Point<f64> {
    Point::new(p.x as f64, p.y as f64)
}

fn view_rect(w: f64, h: f64) -> geo::Rect<f64> {
    let major = w.max(h);
    let vx_min = (w / 2.0) - (major / 2.0);
    let vx_max = (w / 2.0) + (major / 2.0);
    let vy_min = (h / 2.0) - (major / 2.0);
    let vy_max = (h / 2.0) + (major / 2.0);
    geo::Rect::new(Point::new(vx_min, vy_min), Point::new(vx_max, vy_max))
}

fn norm_rect(x: f64, y: f64, zoom: f64, desired_tiles: f64) -> geo::Rect<f64> {
    let nf = 2.0f64.powf(zoom);
    let t_side = 1.0 / nf;
    let east = x - (0.5 * desired_tiles * t_side);
    let west = x + (0.5 * desired_tiles * t_side);
    let north = y + (0.5 * desired_tiles * t_side);
    let south = y - (0.5 * desired_tiles * t_side);

    geo::Rect::new(Point::new(east, north), Point::new(west, south))
}

fn scale(v: f64, src_min: f64, src_max: f64, tgt_min: f64, tgt_max: f64) -> f64 {
    let src_range = src_max - src_min;
    let tgt_range = tgt_max - tgt_min;

    let v = (v - src_min) / src_range;
    v * tgt_range + tgt_min
}

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

fn normalized_mercator(p: Point<f64>) -> Point<f64> {
    let lon = p.x();
    let lat = p.y();

    let x_wm = lon;
    let y_wm = lat.to_radians().tan().asinh();

    let x = 0.5 + (x_wm / 360.0);
    let y = 0.5 - (y_wm / (2.0 * std::f64::consts::PI));

    Point::new(x, y)
}

fn reverse_normalized_mercator(p: Point<f64>) -> Point<f64> {
    let x = p.x();
    let y = p.y();

    let x_wm = (x - 0.5) * 360.0;
    let y_wm = (0.5 - y) * 2.0 * std::f64::consts::PI;

    let lon = x_wm;
    let lat = y_wm.sinh().atan().to_degrees();

    Point::new(lon, lat)
}

fn tile_coords(p: Point<f64>, zoom: u8) -> Point<f64> {
    let projected = normalized_mercator(p);

    let n = 2.0f64.powi(zoom as i32);

    let x_tile = projected.x() * n;
    let y_tile = projected.y() * n;

    Point::new(x_tile, y_tile)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub x: i32,
    pub y: i32,
    pub z: u8,
}

impl TileId {
    fn from_point_and_zoom(p: Point<f64>, zoom: u8) -> Self {
        let coords = tile_coords(p, zoom);
        let x = coords.x() as i32;
        let y = coords.y() as i32;

        Self { x, y, z: zoom }
    }

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

    fn top_left_normalized(&self) -> Point<f64> {
        let x = self.x as f64;
        let y = self.y as f64;

        let n = 2.0f64.powi(self.z as i32);

        let x_tile = x / n;
        let y_tile = y / n;

        Point::new(x_tile, y_tile)
    }

    fn bottom_right_normalized(&self) -> Point<f64> {
        let x = (self.x + 1) as f64;
        let y = (self.y + 1) as f64;

        let n = 2.0f64.powi(self.z as i32);

        let x_tile = x / n;
        let y_tile = y / n;

        Point::new(x_tile, y_tile)
    }

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
