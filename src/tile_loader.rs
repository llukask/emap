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

use crate::TileId;

/// Decoded RGBA8 tile image ready for upload to a GPU texture.
///
/// Pixel data is tightly packed `[r, g, b, a, …]` with `pixels.len() ==
/// width * height * 4`. Width and height are usually equal (256 or 512) but
/// tile providers may serve other sizes, so the renderer reads them from
/// the image.
#[derive(Debug, Clone)]
pub struct TileImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Callback used by an asynchronous [`TileLoader`] to wake the renderer once
/// a tile finishes loading.
///
/// The renderer itself doesn't run an event loop, so loaders need an
/// out-of-band way to nudge the host (e.g. `window.request_redraw()` on
/// winit). Concrete loaders capture this `Arc` and invoke it from their
/// background workers.
pub type RepaintSignal = Arc<dyn Fn() + Send + Sync>;

/// Process-wide fallback loader used when the widget has no explicit one.
///
/// Resolves to [`TokioTileLoader`] when the `tokio` feature is on (the
/// default) and to [`DummyLoader`] otherwise. Lazily constructed on first
/// access so the background thread isn't spawned unless tiles are actually
/// requested. Stored as an [`Arc<dyn TileLoader>`] so it can be handed to
/// [`crate::EMap`] without copying or further wrapping.
#[cfg(feature = "tokio")]
pub static DEFAULT_TILE_LOADER: LazyLock<Arc<dyn TileLoader>> =
    LazyLock::new(|| Arc::new(TokioTileLoader::new()));

/// Process-wide fallback loader (`tokio` feature disabled): always returns
/// the [`DummyLoader`] placeholder image.
#[cfg(not(feature = "tokio"))]
pub static DEFAULT_TILE_LOADER: LazyLock<Arc<dyn TileLoader>> =
    LazyLock::new(|| Arc::new(DummyLoader));

/// Result of a [`TileLoader::tile`] call.
///
/// Distinguishes "image is in flight, please show a loading indicator" from
/// "image is ready to upload" so the renderer can react accordingly.
pub enum TileFetch {
    /// The tile is being fetched (request queued, network in progress, or
    /// decode running). No bytes available this frame.
    Loading,
    /// The tile is decoded and ready to be uploaded as a GPU texture.
    Ready(Arc<TileImage>),
}

/// Strategy for obtaining decoded pixel data for a tile.
///
/// Implementations are expected to be non-blocking from the caller's point of
/// view: return [`TileFetch::Loading`] if the tile isn't ready yet and let a
/// later frame retry. Implementations that fetch asynchronously should invoke
/// the supplied [`RepaintSignal`] once the tile becomes available so the
/// host requests a redraw.
pub trait TileLoader: Send + Sync {
    /// Look up or initiate a fetch for `tile_id`.
    ///
    /// `url` is pre-computed by the [`crate::TileUrlProvider`] so the loader
    /// doesn't need to know about URL templating. `repaint` is held by
    /// async implementations and called when the tile finishes loading.
    fn tile(&self, url: String, tile_id: &TileId, repaint: RepaintSignal) -> TileFetch;
}

/// Stub loader that always returns a fixed 64×64 checkerboard image.
///
/// Useful for offline testing and for documenting the [`TileLoader`] trait
/// without pulling in HTTP. Not useful in production maps.
pub struct DummyLoader;

impl TileLoader for DummyLoader {
    fn tile(&self, _url: String, _tile_id: &TileId, _repaint: RepaintSignal) -> TileFetch {
        const SIZE: u32 = 64;
        let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let on = ((x / 8) + (y / 8)) % 2 == 0;
                let v = if on { 200 } else { 60 };
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        TileFetch::Ready(Arc::new(TileImage {
            width: SIZE,
            height: SIZE,
            pixels,
        }))
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

    /// Format an error together with its full `source()` chain.
    ///
    /// Reqwest's `Display` impl only emits the top-level message
    /// (e.g. `"error sending request"`), so logging `{e}` alone hides
    /// the actual cause (TLS handshake failure, DNS error, etc.). This
    /// walks `Error::source()` so the underlying reason shows up too.
    fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{e}");
        let mut src = e.source();
        while let Some(s) = src {
            let _ = write!(out, ": {s}");
            src = s.source();
        }
        out
    }

    /// Lifecycle of a single tile within a loader's in-memory table.
    ///
    /// Used to deduplicate concurrent requests for the same tile: the first
    /// caller flips the entry to [`Fetch::Pending`] and queues a download,
    /// later callers see `Pending` and back off until [`Fetch::Done`].
    enum Fetch {
        /// A fetch has been kicked off but no image data is available yet.
        Pending,
        /// The tile has been decoded and is ready to be uploaded as a texture.
        Done(Arc<TileImage>),
    }

    type Job = (TileId, String, RepaintSignal);

    /// Decode the response body and store the resulting image, then wake the
    /// host so the next frame uploads it as a texture. On decode failure the
    /// in-flight entry is removed so the next frame re-queues the tile.
    fn decode_and_store(
        bytes: Vec<u8>,
        tile_id: TileId,
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
        repaint: RepaintSignal,
    ) {
        // PNG/JPEG decode is CPU-bound and can stall an async worker; do it
        // on the blocking pool so the tokio reactor stays responsive.
        tokio::task::spawn_blocking(move || {
            let image = match image::load_from_memory(&bytes) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(
                        "tile {:?} decode failed ({} bytes): {}",
                        tile_id,
                        bytes.len(),
                        error_chain(&e)
                    );
                    fail(tile_id, &tiles, &repaint);
                    return;
                }
            };
            let rgba = image.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            let pixels = rgba.into_raw();
            let img = Arc::new(TileImage {
                width,
                height,
                pixels,
            });
            tiles.lock().unwrap().insert(tile_id, Fetch::Done(img));
            repaint();
        });
    }

    /// Drop the pending entry for `tile_id` so a future frame re-requests
    /// the tile, and wake the host to give it a chance to do so.
    fn fail(
        tile_id: TileId,
        tiles: &Mutex<HashMap<TileId, Fetch>>,
        repaint: &RepaintSignal,
    ) {
        tiles.lock().unwrap().remove(&tile_id);
        repaint();
    }

    /// HTTP-fetching [`TileLoader`] backed by a dedicated tokio runtime.
    ///
    /// Spawns a single OS thread that hosts a multi-thread runtime; fetch
    /// jobs are dispatched onto that runtime via an MPSC channel. Decoded
    /// images are stored in an in-memory table keyed by [`TileId`] and
    /// consumed (cloned) by the next call to [`TileLoader::tile`].
    pub struct TokioTileLoader {
        /// Channel into the background runtime; carries the tile id, fetch
        /// URL, and a repaint callback invoked on completion.
        tx: Sender<Job>,
        /// In-flight + completed tiles. Shared with the background tasks.
        tiles: Arc<Mutex<HashMap<TileId, Fetch>>>,
    }

    impl TokioTileLoader {
        /// Spawn the background runtime and return a ready-to-use loader.
        ///
        /// One thread + one runtime is shared across all tile fetches issued
        /// by this instance; per-tile work runs as independent tokio tasks
        /// so requests proceed in parallel up to reqwest's connection limits.
        pub fn new() -> Self {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Job>(1024);
            let tiles = Arc::new(Mutex::new(HashMap::new()));
            let t1 = tiles.clone();
            std::thread::spawn(move || {
                let tiles = t1;
                let rt = tokio::runtime::Runtime::new().unwrap();

                rt.block_on(async move {
                    let client = Arc::new(ClientBuilder::default().build().unwrap());
                    loop {
                        let Some((tile_id, url, repaint)) = rx.recv().await else {
                            break;
                        };
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

                            let r = match client
                                .get(&url)
                                .header("user-agent", user_agent)
                                .send()
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!(
                                        "tile {tile_id:?} request failed ({url}): {}",
                                        error_chain(&e)
                                    );
                                    fail(tile_id, &ts, &repaint);
                                    return;
                                }
                            };
                            let status = r.status();
                            if !status.is_success() {
                                if status.as_u16() == 429 {
                                    tracing::warn!(
                                        "tile {tile_id:?} rate limited (429) by {url}"
                                    );
                                } else {
                                    tracing::warn!(
                                        "tile {tile_id:?} HTTP {} from {url}",
                                        status.as_u16()
                                    );
                                }
                                fail(tile_id, &ts, &repaint);
                                return;
                            }
                            let b = match r.bytes().await {
                                Ok(b) => b.to_vec(),
                                Err(e) => {
                                    tracing::warn!(
                                        "tile {tile_id:?} body read failed: {}",
                                        error_chain(&e)
                                    );
                                    fail(tile_id, &ts, &repaint);
                                    return;
                                }
                            };

                            decode_and_store(b, tile_id, ts, repaint);
                        });
                    }
                });
            });

            TokioTileLoader { tiles, tx }
        }
    }

    impl Default for TokioTileLoader {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TileLoader for TokioTileLoader {
        fn tile(&self, url: String, tile_id: &TileId, repaint: RepaintSignal) -> TileFetch {
            let t = self.tiles.lock().unwrap();
            match t.get(tile_id) {
                Some(Fetch::Pending) => TileFetch::Loading,
                Some(Fetch::Done(c)) => TileFetch::Ready(c.clone()),
                None => {
                    self.tx.blocking_send((*tile_id, url, repaint)).unwrap();
                    TileFetch::Loading
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
    /// in-memory entry: [`TileLoader::tile`] returns the [`Arc<TileImage>`]
    /// once and then drops it, since the renderer caches the uploaded
    /// GPU texture itself. This keeps the loader's in-memory footprint
    /// small over long sessions.
    #[cfg(feature = "caching")]
    pub struct CachingTileLoader {
        tx: Sender<Job>,
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
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Job>(1024);
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
                        let Some((tile_id, url, repaint)) = rx.recv().await else {
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
                            if tokio::fs::metadata(&path).await.is_ok() {
                                match tokio::fs::read(&path).await {
                                    Ok(b) => {
                                        decode_and_store(b, tile_id, ts, repaint);
                                        return;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "tile {tile_id:?} cache read failed ({}): {}",
                                            path.display(),
                                            error_chain(&e)
                                        );
                                        // Fall through to network on cache read error.
                                    }
                                }
                            }

                            let user_agent =
                                format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

                            let r = match client
                                .get(&url)
                                .header("user-agent", user_agent)
                                .send()
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!(
                                        "tile {tile_id:?} request failed ({url}): {}",
                                        error_chain(&e)
                                    );
                                    fail(tile_id, &ts, &repaint);
                                    return;
                                }
                            };
                            let status = r.status();
                            let b = match r.bytes().await {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::warn!(
                                        "tile {tile_id:?} body read failed: {}",
                                        error_chain(&e)
                                    );
                                    fail(tile_id, &ts, &repaint);
                                    return;
                                }
                            };

                            // Only cache successful responses; persisting 4xx/5xx
                            // bodies would poison the cache with non-image data.
                            if status.is_success() {
                                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                                    tracing::warn!(
                                        "tile {tile_id:?} mkdir {} failed: {}",
                                        dir.display(),
                                        error_chain(&e)
                                    );
                                    fail(tile_id, &ts, &repaint);
                                    return;
                                }
                                if let Err(e) = tokio::fs::write(&path, &b).await {
                                    tracing::warn!(
                                        "tile {tile_id:?} cache write {} failed: {}",
                                        path.display(),
                                        error_chain(&e)
                                    );
                                    // Continue: the tile decoded fine, just
                                    // didn't persist. Fall through to decode.
                                }
                                decode_and_store(b.to_vec(), tile_id, ts, repaint);
                            } else {
                                if status.as_u16() == 429 {
                                    tracing::warn!(
                                        "tile {tile_id:?} rate limited (429) by {url}"
                                    );
                                } else {
                                    tracing::warn!(
                                        "tile {tile_id:?} HTTP {} from {url}",
                                        status.as_u16()
                                    );
                                }
                                fail(tile_id, &ts, &repaint);
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
        fn tile(&self, url: String, tile_id: &TileId, repaint: RepaintSignal) -> TileFetch {
            let mut t = self.tiles.lock().unwrap();
            match t.get(tile_id) {
                Some(Fetch::Pending) => TileFetch::Loading,
                Some(Fetch::Done(_)) => match t.remove(tile_id) {
                    Some(Fetch::Done(c)) => TileFetch::Ready(c),
                    _ => TileFetch::Loading,
                },
                None => {
                    self.tx.blocking_send((*tile_id, url, repaint)).unwrap();
                    TileFetch::Loading
                }
            }
        }
    }
}
