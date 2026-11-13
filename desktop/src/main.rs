#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod connections;
mod suggestions;
mod terminal;

use clap::Parser;

fn main() -> eframe::Result<()> {
    let launch = app::Options::parse();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("nosh desktop")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 540.0]),
        ..Default::default()
    };
    eframe::run_native(
        "nosh desktop",
        options,
        Box::new(move |cc| Ok(Box::new(app::Desktop::new(cc, launch)))),
    )
}
