//! Coordinate-system helpers shared by the renderer and the public API.
//!
//! Three coordinate systems are in play:
//!
//! 1. **Geographic** — [`geo::Point<f64>`] of `(lon, lat)` degrees. Public
//!    API surface (shape constructors, [`crate::EMapResponse`]).
//! 2. **Normalized Mercator** — [`geo::Point<f64>`] of `(x, y) ∈ [0, 1]²`,
//!    `(0, 0)` is the world's top-left and `(1, 1)` the bottom-right.
//! 3. **Screen pixels** — [`glam::Vec2`] inside the renderer's allocated
//!    viewport.
//!
//! Conversions: [`normalized_mercator`] / [`reverse_normalized_mercator`]
//! between geographic and normalized Mercator; [`scale`] / [`scale_rect`]
//! between normalized Mercator and pixels.

use geo::Point;

/// Linearly remap `v` from `[src_min, src_max]` into `[tgt_min, tgt_max]`.
///
/// No clamping; values outside the source range extrapolate.
pub fn scale(v: f64, src_min: f64, src_max: f64, tgt_min: f64, tgt_max: f64) -> f64 {
    let src_range = src_max - src_min;
    let tgt_range = tgt_max - tgt_min;
    let v = (v - src_min) / src_range;
    v * tgt_range + tgt_min
}

/// 2D version of [`scale`]: remap a point from one rectangle into another.
pub fn scale_rect(
    pos: Point<f64>,
    src_space: geo::Rect<f64>,
    tgt_space: geo::Rect<f64>,
) -> Point<f64> {
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
/// Latitudes outside ±85.0511° map outside `[0, 1]` (Web Mercator is
/// undefined at the poles).
pub fn normalized_mercator(p: Point<f64>) -> Point<f64> {
    let lon = p.x();
    let lat = p.y();

    let x_wm = lon;
    let y_wm = lat.to_radians().tan().asinh();

    let x = 0.5 + (x_wm / 360.0);
    let y = 0.5 - (y_wm / (2.0 * std::f64::consts::PI));

    Point::new(x, y)
}

/// Inverse of [`normalized_mercator`].
pub fn reverse_normalized_mercator(p: Point<f64>) -> Point<f64> {
    let x = p.x();
    let y = p.y();

    let x_wm = (x - 0.5) * 360.0;
    let y_wm = (0.5 - y) * 2.0 * std::f64::consts::PI;

    let lon = x_wm;
    let lat = y_wm.sinh().atan().to_degrees();

    Point::new(lon, lat)
}

/// Convert a geographic point to fractional tile coordinates at `zoom`.
fn tile_coords(p: Point<f64>, zoom: u8) -> Point<f64> {
    let projected = normalized_mercator(p);
    let n = 2.0f64.powi(zoom as i32);
    Point::new(projected.x() * n, projected.y() * n)
}

/// Compute the screen-space square the map projects into.
///
/// The square is centered in the viewport and uses the longer side of the
/// allocated rectangle so the projection stays isotropic regardless of the
/// host UI's aspect ratio.
pub fn view_rect(w: f64, h: f64) -> geo::Rect<f64> {
    let major = w.max(h);
    let vx_min = (w / 2.0) - (major / 2.0);
    let vx_max = (w / 2.0) + (major / 2.0);
    let vy_min = (h / 2.0) - (major / 2.0);
    let vy_max = (h / 2.0) + (major / 2.0);
    geo::Rect::new(Point::new(vx_min, vy_min), Point::new(vx_max, vy_max))
}

/// Compute the visible rectangle in normalized-Mercator coordinates.
pub fn norm_rect(x: f64, y: f64, zoom: f64, desired_tiles: f64) -> geo::Rect<f64> {
    let nf = 2.0f64.powf(zoom);
    let t_side = 1.0 / nf;
    let east = x - (0.5 * desired_tiles * t_side);
    let west = x + (0.5 * desired_tiles * t_side);
    let north = y + (0.5 * desired_tiles * t_side);
    let south = y - (0.5 * desired_tiles * t_side);
    geo::Rect::new(Point::new(east, north), Point::new(west, south))
}

/// Identifier for a single tile in the XYZ slippy-map scheme.
///
/// At zoom `z` the world is divided into a `2^z × 2^z` grid of tiles, where
/// `x` increases east from longitude 180°W and `y` increases south from the
/// Mercator-clamped north edge. Same convention as OpenStreetMap, Mapbox,
/// Google Maps, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub x: i32,
    pub y: i32,
    pub z: u8,
}

impl TileId {
    /// Tile that contains the given geographic point at the requested zoom.
    pub fn from_point_and_zoom(p: Point<f64>, zoom: u8) -> Self {
        let coords = tile_coords(p, zoom);
        Self {
            x: coords.x() as i32,
            y: coords.y() as i32,
            z: zoom,
        }
    }

    /// Enumerate every tile that intersects the geographic rectangle spanned
    /// by `p1` and `p2` at `zoom`, expanded outward by `padding` tiles on
    /// each side. Tiles outside the valid `0..2^z` range are skipped.
    pub fn from_bounds(p1: Point<f64>, p2: Point<f64>, zoom: u8, padding: i32) -> Vec<Self> {
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
    pub fn top_left_normalized(&self) -> Point<f64> {
        let n = 2.0f64.powi(self.z as i32);
        Point::new(self.x as f64 / n, self.y as f64 / n)
    }

    /// South-east corner of this tile in normalized Mercator space.
    pub fn bottom_right_normalized(&self) -> Point<f64> {
        let n = 2.0f64.powi(self.z as i32);
        Point::new((self.x + 1) as f64 / n, (self.y + 1) as f64 / n)
    }

    /// UV sub-rect within this tile that an axis-aligned child quadrant
    /// covers. Used by the pyramid fallback: starting from a child UV of
    /// `[0,0]..[1,1]`, repeated calls walk up the pyramid, accumulating the
    /// sub-rect that should be sampled from each parent.
    pub fn zoom_out_with_uv(&self, uv: UvRect) -> (TileId, UvRect) {
        let new_tile = TileId {
            x: self.x / 2,
            y: self.y / 2,
            z: self.z - 1,
        };

        // The child's position within its parent: even x → left half,
        // odd x → right half; even y → top half, odd y → bottom half.
        let off_x = if self.x % 2 == 0 { 0.0 } else { 0.5 };
        let off_y = if self.y % 2 == 0 { 0.0 } else { 0.5 };

        let half_w = uv.max.x - uv.min.x;
        let half_h = uv.max.y - uv.min.y;

        let new_min = glam::Vec2::new(
            uv.min.x + off_x * half_w,
            uv.min.y + off_y * half_h,
        );
        let new_max = glam::Vec2::new(new_min.x + 0.5 * half_w, new_min.y + 0.5 * half_h);
        (new_tile, UvRect { min: new_min, max: new_max })
    }
}

/// UV sub-rectangle into a tile texture. `[0,0]..[1,1]` is the full image.
#[derive(Debug, Clone, Copy)]
pub struct UvRect {
    pub min: glam::Vec2,
    pub max: glam::Vec2,
}

impl UvRect {
    pub const FULL: UvRect = UvRect {
        min: glam::Vec2::ZERO,
        max: glam::Vec2::ONE,
    };
}
