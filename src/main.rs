#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

mod app;
mod device;
mod imaging;
mod platform;
mod proto;
mod schedule;
mod settings;
#[cfg(windows)]
mod winserial;
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
    log.info("launching");
    {
        // Windows GUI builds have no console: record panics in the log file instead.
        let l = log.clone();
        std::panic::set_hook(Box::new(move |info| l.error(format!("panic: {info}"))));
    }
    let start_hidden = std::env::args().any(|a| a == "--minimized");

    let (rgba, w, h) = platform::icon_rgba();
    let viewport = egui::ViewportBuilder::default()
        .with_title("Z-NEO Manager")
        .with_inner_size([900.0, 620.0])
        .with_min_inner_size([760.0, 520.0])
        .with_icon(egui::IconData { rgba, width: w, height: h })
        .with_visible(!start_hidden);

    let app_log = log.clone();
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native(
        "Z-NEO Manager",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths, app_log, start_hidden)))),
    );
    drop(guard);
    if let Err(e) = &result {
        log.error(format!("window/graphics start failed: {e}"));
    }
    result
}
