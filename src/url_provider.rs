use crate::TileId;

pub trait TileUrlProvider {
    fn url(&self, tile_id: TileId) -> String;
}

impl<O, F> TileUrlProvider for F
where
    O: ToString,
    F: Fn(&TileId) -> O,
{
    fn url(&self, tile_id: TileId) -> String {
        self(&tile_id).to_string()
    }
}

pub struct MapBoxTileUrlProvider {
    token: String,
    style: String,
}

impl MapBoxTileUrlProvider {
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
