#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod device;
mod imaging;
mod platform;
mod proto;
mod schedule;
mod settings;
mod worker;

use eframe::egui;
use std::sync::Arc;

fn main() -> eframe::Result {
    let paths = settings::Paths::resolve();
    let _ = paths.ensure_dirs();

    // Only one instance may own the device; a second launch just asks the first to show itself.
    let guard = single_instance::SingleInstance::new("ZNeoManager-8f3a2c1e").ok();
    if let Some(g) = &guard {
        if !g.is_single() {
            let _ = std::fs::write(paths.show_request_file(), b"show");
            return Ok(());
        }
    }

    let log = Arc::new(settings::Logger::new(paths.log_file()));
    let start_hidden = std::env::args().any(|a| a == "--minimized");

    let (rgba, w, h) = platform::icon_rgba();
    let viewport = egui::ViewportBuilder::default()
        .with_title("Z-NEO Manager")
        .with_inner_size([900.0, 620.0])
        .with_min_inner_size([760.0, 520.0])
        .with_icon(egui::IconData { rgba, width: w, height: h })
        .with_visible(!start_hidden);

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native(
        "Z-NEO Manager",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths, log, start_hidden)))),
    );
    drop(guard);
    result
}
