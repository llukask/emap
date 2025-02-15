use egui::{Color32, Stroke};
use emap::CachingTileLoader;
use geo::Point;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::viewport::ViewportBuilder::default().with_title("emap example"),
        ..Default::default()
    };
    let tile_loader = CachingTileLoader::new("./cache");
    eframe::run_native(
        "eframe template",
        native_options,
        Box::new(|_cc| Ok(Box::new(EMapApp { tile_loader }))),
    )?;

    Ok(())
}

struct EMapApp {
    tile_loader: CachingTileLoader,
}

impl eframe::App for EMapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(&ctx, |ui| {
            let line_start = Point::new(16.340083, 48.179349);
            let line_end = Point::new(16.341451, 48.176684);

            let mut map = emap::EMap::new("map")
                .initial_position(ctx, 48.178993463351695, 16.340540441879874, 12)
                .tile_size(256.0)
                .tile_loader(&self.tile_loader);
            map = map.line(
                line_start,
                line_end,
                Stroke::new(4.0, Color32::GOLD.gamma_multiply(0.75)),
            );

            ui.add(map);
        });
    }
}
