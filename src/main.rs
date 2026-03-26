mod app;
mod svg_doc;
mod parser;
mod renderer;
mod elements_pane;
mod spatial_index;
mod clip_index;
mod filter;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SVG Viewer")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "SVG Viewer",
        native_options,
        Box::new(|cc| Ok(Box::new(app::SvgViewerApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
