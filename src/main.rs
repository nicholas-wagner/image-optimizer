#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyze;
mod app;
mod bootstrap;
mod commands;
mod execute;
mod files;
mod licenses;
mod model;
mod settings;
mod tools;
mod update;

use eframe::egui;

fn main() -> eframe::Result {
    bootstrap::ensure_runtime();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 800.0])
            .with_min_inner_size([900.0, 640.0])
            .with_drag_and_drop(true)
            .with_title("Image Optimizer"),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "Image Optimizer",
        options,
        Box::new(|cc| Ok(Box::new(app::OptimizerApp::new(cc)))),
    )
}
