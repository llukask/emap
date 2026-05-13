//! Minimal eframe app demonstrating the `emap` widget: a map of Vienna with
//! a fixed gold line and a user-editable red polyline (left-click to add a
//! point, right-click to pop the last one).

use egui::{Color32, Stroke};
use emap::CachingTileLoader;
use geo::Point;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::viewport::ViewportBuilder::default().with_title("emap example"),
        ..Default::default()
    };

    // Persist tiles under ./cache so repeat launches don't re-download from OSM.
    let tile_loader = CachingTileLoader::new("./cache");
    eframe::run_native(
        "eframe template",
        native_options,
        Box::new(|_cc| {
            Ok(Box::new(EMapApp {
                tile_loader,
                points: vec![],
            }))
        }),
    )?;

    Ok(())
}

struct EMapApp {
    // Owned by the app so it outlives every per-frame `EMap` builder; the
    // widget only borrows it.
    tile_loader: CachingTileLoader,

    /// Polyline points collected from user clicks, in lon/lat.
    points: Vec<Point<f64>>,
}

impl eframe::App for EMapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let line_start = Point::new(16.340083, 48.179349);
            let line_end = Point::new(16.341451, 48.176684);

            // `initial_position` only applies on the first frame, so panning
            // and zooming the map afterwards is preserved across updates.
            let mut map = emap::EMap::new("map")
                .initial_position(ctx, 48.178993463351695, 16.340540441879874, 12)
                .tile_size(256.0)
                .tile_loader(&self.tile_loader);
            map = map.line(
                line_start,
                line_end,
                Stroke::new(4.0, Color32::GOLD.gamma_multiply(0.75)),
            );

            // Small filled triangle near the gold line so the polygon
            // primitive has a visible demonstration on screen.
            let triangle = vec![
                Point::new(16.339_000, 48.180_000),
                Point::new(16.342_500, 48.179_500),
                Point::new(16.340_500, 48.177_500),
            ];
            map = map.polygon(
                triangle,
                Some(Stroke::new(2.0, Color32::WHITE)),
                Some(Color32::BLUE.gamma_multiply(0.35)),
            );

            for p in &self.points {
                map = map.filled_circle(*p, 10.0, Color32::RED);
            }

            map = map.line_string(self.points.clone(), Stroke::new(2.0, Color32::GREEN));

            let r = map.show(ui);
            // Left-click appends the cursor's geographic position; right-click pops.
            if r.clicked() {
                if let Some(pos) = r.pointer_position() {
                    self.points.push(pos);
                }
            } else if r.secondary_clicked() {
                self.points.pop();
            }
        });
    }
}
