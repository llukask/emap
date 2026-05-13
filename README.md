# emap

Slippy-map renderer for [wgpu](https://wgpu.rs). Raster XYZ tiles on a
Web-Mercator projection, with pan, wheel zoom, and overlay primitives
(line, line string, circle, polygon).

The renderer is bring-your-own-surface: the caller supplies an active
`wgpu::RenderPass` each frame. Asynchronous tile fetching ships with
the crate behind feature flags.

## Quick start

```rust
use std::sync::Arc;
use emap::{EMap, Frame, Input, Viewport};

let repaint: emap::RepaintSignal = Arc::new(|| { /* wake host */ });
let mut emap = EMap::new(&device, surface_format, repaint);
emap.set_initial_position(52.5, 13.4, 8); // lat, lon, zoom

// Per frame, inside a render pass for the surface texture:
let response = emap.render(
    &Frame {
        device: &device,
        queue: &queue,
        viewport: Viewport { origin: glam::Vec2::ZERO, size: window_size },
        input: Input { pointer_position, scroll_delta_y, drag_delta },
        shapes: &[],
    },
    &mut render_pass,
);
```

## Examples

- `cargo run --example winit` — standalone winit + wgpu host.
- `cargo run --example eframe --features egui-wgpu` — embedded inside
  an eframe app via the `EmapHandle` helper.
- `MAPBOX_TOKEN=pk.your_token cargo run --example mapbox --features egui-wgpu`
  — same as the eframe example, but sources tiles from the Mapbox
  Static Tiles API. Override the style with `MAPBOX_STYLE`
  (default: `mapbox/streets-v12`).

## Feature flags

| Flag        | Default | Purpose                                                              |
| ----------- | :-----: | -------------------------------------------------------------------- |
| `tokio`     |    ✓    | Async HTTP `TokioTileLoader` (default).                              |
| `caching`   |    ✓    | On-disk tile cache (`CachingTileLoader`). Implies `tokio`.            |
| `egui-wgpu` |         | `emap::egui_wgpu::EmapHandle` for embedding in egui-wgpu hosts.       |

With no features enabled only `DummyLoader` is available; the renderer
compiles but has no real tile source.

## Verify the build

```sh
cargo build --all-features
cargo build --no-default-features --features tokio
cargo build --no-default-features
cargo clippy --all-features --all-targets
```

## Tile providers

`OsmStandardTileUrlProvider` (default) and `MapBoxTileUrlProvider` ship
in the crate. Any `Fn(&TileId) -> impl ToString` also satisfies
`TileUrlProvider` for ad-hoc endpoints. Both bundled HTTP loaders set a
`User-Agent` of `emap/<version>` — OSM's tile policy and most other
providers require client identification.
