//! eframe demo for the `emap` crate.
//!
//! Uses the `emap::egui_wgpu` integration to embed the map inside an
//! egui frame. Left-click adds a polygon vertex, right-click pops, the
//! scroll-wheel zooms toward the cursor, and dragging with the left
//! button pans. A floating "viewport" window reads the latest
//! `EMapResponse` and prints the center / zoom / cursor / bounds.
//!
//! Run with `cargo run --example eframe --features egui-wgpu`.

use std::sync::Arc;

use eframe::egui;
use emap::egui_wgpu::EmapHandle;
use emap::{CachingTileLoader, Color, EMapResponse, Shape, Stroke};

fn main() -> Result<(), eframe::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,emap=info")),
        )
        .init();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "emap — eframe demo",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

struct App {
    emap: EmapHandle,
    polygon_points: Vec<geo::Point<f64>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("eframe Wgpu renderer required");

        let emap = EmapHandle::install(rs, &cc.egui_ctx);
        emap.with(|e| {
            e.set_tile_loader(Arc::new(CachingTileLoader::new("cache")));
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
    fn ui(&mut self, ui: &mut egui::Ui, _eframe_frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
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
            viewport_window(&ctx, &r, self.polygon_points.len());
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

                    let v = r.visible_bounds();
                    ui.label("visible");
                    ui.monospace(format!(
                        "{:.4}…{:.4}, {:.4}…{:.4}",
                        v.min().y,
                        v.max().y,
                        v.min().x,
                        v.max().x,
                    ));
                    ui.end_row();

                    let p = r.projected_bounds();
                    ui.label("projected");
                    ui.monospace(format!(
                        "{:.4}…{:.4}, {:.4}…{:.4}",
                        p.min().y,
                        p.max().y,
                        p.min().x,
                        p.max().x,
                    ));
                    ui.end_row();

                    ui.label("in-flight tiles");
                    ui.monospace(format!("{}", r.in_flight_tiles()));
                    ui.end_row();
                });

            ui.separator();
            ui.label(format!("polygon points: {polygon_points}"));
            ui.label("left-click: add point · right-click: pop");
        });
}
