use std::sync::{Arc, LazyLock};

use egui::{ColorImage, Context};

use crate::TileId;

#[cfg(feature = "tokio")]
pub static DEFAULT_TILE_LOADER: LazyLock<TokioTileLoader> = LazyLock::new(TokioTileLoader::new);

#[cfg(not(feature = "tokio"))]
pub static DEFAULT_TILE_LOADER: LazyLock<DummyLoader> = LazyLock::new(|| DummyLoader);

pub trait TileLoader {
    fn tile(&self, url: String, tile_id: &TileId, ctx: Context) -> Option<Arc<ColorImage>>;
}

/// This loader just loads the egui::ColorImage example image, which isn't very useful.
pub struct DummyLoader;

impl TileLoader for DummyLoader {
    fn tile(&self, _url: String, _tile_id: &TileId, _ctx: Context) -> Option<Arc<ColorImage>> {
        let img = ColorImage::example();
        Some(Arc::new(img))
    }
}

#[cfg(feature = "tokio")]
pub use tokio_loader::*;

#[cfg(feature = "tokio")]
mod tokio_loader {
    use std::{collections::HashMap, sync::Mutex};

    use reqwest::ClientBuilder;
    use tokio::sync::mpsc::Sender;

    use super::*;

    enum Fetch {
        Pending,
        Done(Arc<ColorImage>),
    }

    #[cfg(feature = "tokio")]
    pub struct TokioTileLoader {
        tx: Sender<(TileId, String, Context)>,
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
    }

    #[cfg(feature = "tokio")]
    impl TokioTileLoader {
        pub fn new() -> Self {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            let tiles = Arc::new(Mutex::new(HashMap::new()));
            let t1 = tiles.clone();
            std::thread::spawn(move || {
                let tiles = t1;
                let rt = tokio::runtime::Runtime::new().unwrap();

                rt.block_on(async move {
                    let client = Arc::new(ClientBuilder::default().build().unwrap());
                    loop {
                        let (tile_id, url, ctx): (TileId, String, Context) =
                            rx.recv().await.unwrap();
                        let ts = tiles.clone();
                        {
                            ts.lock().unwrap().insert(tile_id, Fetch::Pending);
                        }
                        let ts = tiles.clone();
                        let c = client.clone();
                        tokio::spawn(async move {
                            let client = c;

                            let user_agent =
                                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

                            let r = client
                                .get(url)
                                .header("user-agent", user_agent)
                                .send()
                                .await
                                .unwrap();
                            let b = r.bytes().await.unwrap();

                            tokio::task::spawn_blocking(move || {
                                let image = image::load_from_memory(&b.clone()).unwrap();
                                let size = [image.width() as _, image.height() as _];
                                let image_buffer = image.to_rgba8();
                                let pixels = image_buffer.as_flat_samples();

                                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                    size,
                                    pixels.as_slice(),
                                );

                                ts.lock()
                                    .unwrap()
                                    .insert(tile_id, Fetch::Done(color_image.into()));
                                ctx.request_repaint();
                            });
                        });
                    }
                });
            });

            TokioTileLoader { tiles, tx }
        }
    }

    #[cfg(feature = "tokio")]
    impl Default for TokioTileLoader {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(feature = "tokio")]
    impl TileLoader for TokioTileLoader {
        fn tile(&self, url: String, tile_id: &TileId, ctx: Context) -> Option<Arc<ColorImage>> {
            let t = self.tiles.lock().unwrap();
            match t.get(tile_id) {
                Some(Fetch::Pending) => None,
                Some(Fetch::Done(c)) => Some(c.clone()),
                None => {
                    self.tx.blocking_send((*tile_id, url, ctx)).unwrap();
                    None
                }
            }
        }
    }

    #[cfg(feature = "caching")]
    pub struct CachingTileLoader {
        tx: Sender<(TileId, String, Context)>,
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
    }

    #[cfg(feature = "caching")]
    impl CachingTileLoader {
        pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(TileId, String, Context)>(1024);
            let tiles = Arc::new(Mutex::new(HashMap::new()));
            let t1 = tiles.clone();

            let cache_dir = dir.into();
            let c = cache_dir.clone();
            std::thread::spawn(move || {
                let cache_dir = c;
                let tiles = t1;
                let rt = tokio::runtime::Runtime::new().unwrap();

                rt.block_on(async move {
                    let client = Arc::new(ClientBuilder::default().build().unwrap());
                    loop {
                        let Some((tile_id, url, ctx)) = rx.recv().await else {
                            break;
                        };
                        let ts = tiles.clone();
                        {
                            ts.lock().unwrap().insert(tile_id, Fetch::Pending);
                        }
                        let ts = tiles.clone();
                        let c = client.clone();
                        let cd = cache_dir.clone();
                        tokio::spawn(async move {
                            let client = c;
                            let cache_dir = cd;

                            let path = cache_dir
                                .join(format!("{}/{}/{}", tile_id.z, tile_id.x, tile_id.y));
                            let dir = path.parent().unwrap();

                            let exists = tokio::fs::metadata(&path).await;
                            let exists = exists.is_ok();
                            if exists {
                                let b =
                                    bytes::Bytes::from_owner(tokio::fs::read(&path).await.unwrap());

                                let ctx = ctx.clone();
                                let ts = ts.clone();
                                tokio::task::spawn_blocking(move || {
                                    let image = image::load_from_memory(&b.clone()).unwrap();
                                    let size = [image.width() as _, image.height() as _];
                                    let image_buffer = image.to_rgba8();
                                    let pixels = image_buffer.as_flat_samples();

                                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                        size,
                                        pixels.as_slice(),
                                    );

                                    ts.lock()
                                        .unwrap()
                                        .insert(tile_id, Fetch::Done(color_image.into()));
                                    ctx.request_repaint();
                                });
                                return;
                            }

                            let user_agent =
                                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

                            let r = client
                                .get(url)
                                .header("user-agent", user_agent)
                                .send()
                                .await
                                .unwrap();
                            let status = r.status();
                            let b = r.bytes().await.unwrap();

                            if status.is_success() {
                                tokio::fs::create_dir_all(&dir).await.unwrap();
                                tokio::fs::write(path, b.clone()).await.unwrap();

                                tokio::task::spawn_blocking(move || {
                                    let image = image::load_from_memory(&b.clone()).unwrap();
                                    let size = [image.width() as _, image.height() as _];
                                    let image_buffer = image.to_rgba8();
                                    let pixels = image_buffer.as_flat_samples();

                                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                        size,
                                        pixels.as_slice(),
                                    );

                                    ts.lock()
                                        .unwrap()
                                        .insert(tile_id, Fetch::Done(color_image.into()));
                                    ctx.request_repaint();
                                });
                            }
                        });
                    }
                });
            });

            CachingTileLoader { tiles, tx }
        }
    }

    #[cfg(feature = "caching")]
    impl TileLoader for CachingTileLoader {
        fn tile(&self, url: String, tile_id: &TileId, ctx: Context) -> Option<Arc<ColorImage>> {
            let t = self.tiles.lock().unwrap();
            match t.get(tile_id) {
                Some(Fetch::Pending) => None,
                Some(Fetch::Done(c)) => Some(c.clone()),
                None => {
                    self.tx.blocking_send((*tile_id, url, ctx)).unwrap();
                    None
                }
            }
        }
    }
}
