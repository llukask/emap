//! Tile loading strategies.
//!
//! [`TileLoader`] is the abstraction over "how do we get the pixels for a
//! tile?". Implementations may fetch over HTTP, read from disk, or return a
//! stub image. The crate ships:
//!
//! - [`DummyLoader`] — returns a fixed test image; useful for offline demos.
//! - [`TokioTileLoader`] (feature `tokio`) — async HTTP fetch in a background
//!   runtime, with in-memory deduplication of in-flight requests.
//! - [`CachingTileLoader`] (feature `caching`) — same as `TokioTileLoader`
//!   plus a disk cache so repeat runs hit the local filesystem instead of
//!   the remote server.
//!
//! [`DEFAULT_TILE_LOADER`] is selected by feature flag and used whenever the
//! widget is shown without an explicit loader.

use std::sync::{Arc, LazyLock};

use egui::{ColorImage, Context};

use crate::TileId;

/// Process-wide fallback loader used when the widget has no explicit one.
///
/// Resolves to [`TokioTileLoader`] when the `tokio` feature is on (the
/// default) and to [`DummyLoader`] otherwise. Lazily constructed on first
/// access so the background thread isn't spawned unless tiles are actually
/// requested.
#[cfg(feature = "tokio")]
pub static DEFAULT_TILE_LOADER: LazyLock<TokioTileLoader> = LazyLock::new(TokioTileLoader::new);

/// Process-wide fallback loader (`tokio` feature disabled): always returns
/// the [`DummyLoader`] placeholder image.
#[cfg(not(feature = "tokio"))]
pub static DEFAULT_TILE_LOADER: LazyLock<DummyLoader> = LazyLock::new(|| DummyLoader);

/// Strategy for obtaining decoded pixel data for a tile.
///
/// Implementations are expected to be non-blocking from the caller's point of
/// view: return `None` if the tile isn't ready yet and let a later frame
/// retry. Implementations that fetch asynchronously should call
/// `ctx.request_repaint()` once the tile becomes available so the UI picks
/// it up promptly.
pub trait TileLoader {
    /// Look up or initiate a fetch for `tile_id`.
    ///
    /// `url` is pre-computed by the [`crate::TileUrlProvider`] so the loader
    /// doesn't need to know about URL templating. `ctx` is captured by async
    /// implementations to request a repaint when the tile finishes loading.
    fn tile(&self, url: String, tile_id: &TileId, ctx: Context) -> Option<Arc<ColorImage>>;
}

/// Stub loader that always returns [`ColorImage::example`].
///
/// Useful for offline testing and for documenting the [`TileLoader`] trait
/// without pulling in HTTP. Not useful in production maps.
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

    /// Lifecycle of a single tile within a loader's in-memory table.
    ///
    /// Used to deduplicate concurrent requests for the same tile: the first
    /// caller flips the entry to [`Fetch::Pending`] and queues a download,
    /// later callers see `Pending` and back off until [`Fetch::Done`].
    enum Fetch {
        /// A fetch has been kicked off but no image data is available yet.
        Pending,
        /// The tile has been decoded and is ready to be uploaded as a texture.
        Done(Arc<ColorImage>),
    }

    /// HTTP-fetching [`TileLoader`] backed by a dedicated tokio runtime.
    ///
    /// Spawns a single OS thread that hosts a multi-thread runtime; fetch
    /// jobs are dispatched onto that runtime via an MPSC channel. Decoded
    /// images are stored in an in-memory table keyed by [`TileId`] and
    /// consumed (cloned) by the next call to [`TileLoader::tile`].
    #[cfg(feature = "tokio")]
    pub struct TokioTileLoader {
        /// Channel into the background runtime; carries the tile id, fetch
        /// URL, and an [`egui::Context`] used to request a repaint on
        /// completion.
        tx: Sender<(TileId, String, Context)>,
        /// In-flight + completed tiles. Shared with the background tasks.
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
    }

    #[cfg(feature = "tokio")]
    impl TokioTileLoader {
        /// Spawn the background runtime and return a ready-to-use loader.
        ///
        /// One thread + one runtime is shared across all tile fetches issued
        /// by this instance; per-tile work runs as independent tokio tasks
        /// so requests proceed in parallel up to reqwest's connection limits.
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

                            // OSM tile-usage policy (and most providers) require
                            // identifying the client via the User-Agent header,
                            // so we send the crate's name + version.
                            let user_agent =
                                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

                            let r = client
                                .get(url)
                                .header("user-agent", user_agent)
                                .send()
                                .await
                                .unwrap();
                            let b = r.bytes().await.unwrap();

                            // PNG/JPEG decode is CPU-bound and can stall an
                            // async worker; do it on the blocking pool so
                            // the tokio reactor stays responsive.
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

    /// HTTP-fetching loader with a persistent on-disk tile cache.
    ///
    /// Tiles are stored under `<dir>/<z>/<x>/<y>` (no file extension; the
    /// bytes are written verbatim as received from the server). On a cache
    /// hit the network is skipped entirely, which makes repeat runs much
    /// faster and reduces load on third-party tile servers.
    ///
    /// Unlike [`TokioTileLoader`], a successful tile lookup *consumes* the
    /// in-memory entry: [`TileLoader::tile`] returns the [`Arc<ColorImage>`]
    /// once and then drops it, since the [`crate::EMap`] widget caches the
    /// uploaded [`egui::TextureHandle`] itself. This keeps the loader's
    /// in-memory footprint small over long sessions.
    #[cfg(feature = "caching")]
    pub struct CachingTileLoader {
        /// Channel into the background runtime; same protocol as
        /// [`TokioTileLoader::tx`].
        tx: Sender<(TileId, String, Context)>,
        /// Short-lived in-memory hand-off table (entries are removed once
        /// the widget picks them up).
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
    }

    #[cfg(feature = "caching")]
    impl CachingTileLoader {
        /// Create a caching loader rooted at `dir`.
        ///
        /// The directory is created on demand; pass any path you control.
        /// There is no automatic eviction, so the cache grows monotonically
        /// — clean it manually if disk usage becomes a concern.
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

                            // Disk-cache hit: read the previously stored bytes
                            // and skip the network entirely.
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

                            // Only cache successful responses; persisting 4xx/5xx
                            // bodies would poison the cache with non-image data.
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
            let mut t = self.tiles.lock().unwrap();
            match t.get(tile_id) {
                Some(Fetch::Pending) => None,
                Some(Fetch::Done(_)) => t.remove(tile_id).and_then(|f| {
                    if let Fetch::Done(c) = f {
                        Some(c)
                    } else {
                        None
                    }
                }),
                None => {
                    self.tx.blocking_send((*tile_id, url, ctx)).unwrap();
                    None
                }
            }
        }
    }
}
