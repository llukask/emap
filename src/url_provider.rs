//! Strategies for turning a [`TileId`] into a fetch URL.
//!
//! Splitting URL construction from the [`crate::TileLoader`] means the same
//! HTTP/caching plumbing can target any XYZ-style tile service simply by
//! swapping the provider. The crate ships providers for OpenStreetMap and
//! Mapbox; any `Fn(&TileId) -> impl ToString` also satisfies the trait so
//! ad-hoc closures work for one-off endpoints.

use crate::TileId;

/// Strategy for turning a [`TileId`] into the URL the tile loader will fetch.
pub trait TileUrlProvider: Send + Sync {
    /// Format the URL for `tile_id`.
    fn url(&self, tile_id: TileId) -> String;
}

/// Blanket implementation so any `Fn(&TileId) -> impl ToString` can be used
/// directly as a provider — handy for one-off custom tile servers without
/// defining a dedicated struct.
impl<O, F> TileUrlProvider for F
where
    O: ToString,
    F: Fn(&TileId) -> O + Send + Sync,
{
    fn url(&self, tile_id: TileId) -> String {
        self(&tile_id).to_string()
    }
}

/// URL provider for the [Mapbox Static Tiles API].
///
/// Authentication uses a per-account access token; the `style` identifies a
/// published Mapbox style (e.g. `"mapbox/streets-v12"`). See the Mapbox docs
/// for pricing and usage limits.
///
/// [Mapbox Static Tiles API]: https://docs.mapbox.com/api/maps/static-tiles/
pub struct MapBoxTileUrlProvider {
    /// Mapbox access token; appended as the `access_token` query parameter.
    token: String,
    /// Style id of the form `<account>/<style-id>`.
    style: String,
}

impl MapBoxTileUrlProvider {
    /// Construct a provider for the given Mapbox style and access token.
    pub fn new(token: &str, style: &str) -> Self {
        Self {
            token: token.to_string(),
            style: style.to_string(),
        }
    }
}

impl TileUrlProvider for MapBoxTileUrlProvider {
    fn url(&self, tile_id: TileId) -> String {
        format!(
            "https://api.mapbox.com/styles/v1/{}/tiles/{}/{}/{}?access_token={}",
            self.style, tile_id.z, tile_id.x, tile_id.y, self.token
        )
    }
}

/// URL provider for OpenStreetMap's standard raster tile server.
///
/// This is the default provider used by [`crate::EMap::new`]. The public OSM
/// tile server has a strict [tile usage policy] — heavy use, embedded apps,
/// and commercial products should switch to a different provider or run
/// their own cache.
///
/// [tile usage policy]: https://operations.osmfoundation.org/policies/tiles/
#[derive(Default)]
pub struct OsmStandardTileUrlProvider;

impl TileUrlProvider for OsmStandardTileUrlProvider {
    fn url(&self, tile_id: TileId) -> String {
        format!(
            "https://tile.openstreetmap.org/{}/{}/{}.png",
            tile_id.z, tile_id.x, tile_id.y
        )
    }
}
