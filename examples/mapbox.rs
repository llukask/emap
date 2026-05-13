//! Mapbox eframe demo for the `emap` crate.
//!
//! Same controls as `examples/eframe.rs`, but the tile source is swapped to
//! the Mapbox Static Tiles API via [`MapBoxTileUrlProvider`]. The access
//! token is read from the `MAPBOX_TOKEN` env var; an optional style id can
//! be supplied through `MAPBOX_STYLE` (default: `mapbox/streets-v12`).
//!
//! Run with:
//!
//! ```sh
//! MAPBOX_TOKEN=pk.your_token cargo run --example mapbox --features egui-wgpu
//! ```

use std::sync::Arc;

use eframe::egui;
use emap::egui_wgpu::EmapHandle;
use emap::{CachingTileLoader, Color, EMapResponse, MapBoxTileUrlProvider, Shape, Stroke};

fn main() -> Result<(), eframe::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,emap=info")),
        )
        .init();

    let token = std::env::var("MAPBOX_TOKEN").unwrap_or_else(|_| {
        eprintln!("error: MAPBOX_TOKEN env var is required");
        eprintln!("       get a token at https://account.mapbox.com/access-tokens/");
        std::process::exit(1);
    });
    let style =
        std::env::var("MAPBOX_STYLE").unwrap_or_else(|_| "mapbox/streets-v12".to_string());

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "emap — mapbox demo",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, &token, &style)))),
    )
}

struct App {
    emap: EmapHandle,
    polygon_points: Vec<geo::Point<f64>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, token: &str, style: &str) -> Self {
        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("eframe Wgpu renderer required");

        let emap = EmapHandle::install(rs, &cc.egui_ctx);
        emap.with(|e| {
            // Separate cache dir from the OSM example so the two providers
            // don't share tiles on disk.
            e.set_tile_loader(Arc::new(CachingTileLoader::new("cache-mapbox")));
            e.set_tile_url_provider(Arc::new(MapBoxTileUrlProvider::new(token, style)));
            e.set_initial_position(52.5, 13.4, 8);
        });

        Self {
            emap,
            polygon_points: Vec::new(),
        }
    }

    fn shapes(&self) -> Vec<Shape> {
        if self.polygon_points.len() >= 3 {
            vec![Shape::polygon(
                self.polygon_points.clone(),
                Some(Stroke::new(2.0, Color::WHITE)),
                Some(Color::rgba(220, 40, 40, 96)),
            )]
        } else if self.polygon_points.len() == 2 {
            vec![Shape::line(
                self.polygon_points[0],
                self.polygon_points[1],
                Stroke::new(2.0, Color::WHITE),
            )]
        } else {
            Vec::new()
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _eframe_frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let (rect, response) = self.emap.show(ui, self.shapes());

                if response.clicked()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let geo = self.emap.screen_to_geo(
                        pos - rect.min,
                        rect.size(),
                        ctx.pixels_per_point(),
                    );
                    self.polygon_points.push(geo);
                }
                if response.secondary_clicked() {
                    self.polygon_points.pop();
                }
            });

        if let Some(r) = self.emap.last_response() {
            viewport_window(ctx, &r, self.polygon_points.len());
        }
    }
}

fn viewport_window(ctx: &egui::Context, r: &EMapResponse, polygon_points: usize) {
    egui::Window::new("viewport")
        .resizable(false)
        .default_pos(egui::pos2(12.0, 12.0))
        .show(ctx, |ui| {
            egui::Grid::new("emap_response_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label("center");
                    ui.monospace(format!("{:.5}, {:.5}", r.center().y(), r.center().x()));
                    ui.end_row();

                    ui.label("zoom");
                    ui.monospace(format!("{:.3}", r.zoom()));
                    ui.end_row();

                    ui.label("cursor");
                    match r.pointer_position() {
                        Some(p) => ui.monospace(format!("{:.5}, {:.5}", p.y(), p.x())),
                        None => ui.monospace("—"),
                    };
                    ui.end_row();
                });

            ui.separator();
            ui.label(format!("polygon points: {polygon_points}"));
            ui.label("left-click: add point · right-click: pop");
        });
}
