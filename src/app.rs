// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! egui user interface.

use eframe::egui::{self, Color32, RichText, Vec2};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::imaging;
use crate::platform::{self, Tray, TrayCmd};
use crate::proto::{DISPLAY_HEIGHT, DISPLAY_WIDTH};
use crate::schedule::{self, Order};
use crate::settings::{Level, Logger, Paths, Settings, State};
use crate::worker::{file_name, Cmd, ConfigEdit, DeviceStatus, Event, Worker};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn t(zh: bool, en: &'static str, z: &'static str) -> &'static str {
    if zh { z } else { en }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Auto,
    Upload,
    Device,
    Backup,
    Log,
    About,
}

enum UiMsg {
    Tray(TrayCmd),
    FolderPicked(PathBuf),
    FilesPicked(Vec<PathBuf>),
    RestorePicked(PathBuf),
    Preview { path: PathBuf, crop: bool, rgba: Result<Vec<u8>, String> },
    FolderCount { key: (String, bool), count: Result<usize, String> },
}

pub struct App {
    paths: Paths,
    shared_settings: Arc<Mutex<Settings>>,
    state: Arc<Mutex<State>>,
    log: Arc<Logger>,
    cfg: Settings,
    saved_cfg: Settings,

    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Event>,
    ui_tx: Sender<UiMsg>,
    ui_rx: Receiver<UiMsg>,
    tray: Option<Tray>,

    page: Page,
    status: DeviceStatus,
    busy: Option<String>,
    progress: Option<(u64, u64)>,
    quitting: bool,
    quit_confirm: bool,
    /// Remaining attempts to bring a hidden window to the front (commands sent while
    /// hidden can need a further event-loop pass before they take effect).
    show_pending: u8,

    upload_list: Vec<PathBuf>,
    selected: Option<usize>,
    upload_crop: bool,
    preview: Option<(PathBuf, bool, egui::TextureHandle)>,
    preview_pending: Option<(PathBuf, bool)>,
    preview_err: Option<String>,

    edit: ConfigEdit,
    edit_dirty: bool,

    restore_confirm: Option<PathBuf>,
    autostart: bool,
    autostart_err: Option<String>,
    folder_key: Option<(String, bool)>,
    folder_count: Option<Result<usize, String>>,
    ports: Vec<(String, String)>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        paths: Paths,
        log: Arc<Logger>,
        start_hidden: bool,
    ) -> App {
        let ctx = cc.egui_ctx.clone();
        setup_style(&ctx);

        let cfg = Settings::load(&paths);
        let shared_settings = Arc::new(Mutex::new(cfg.clone()));
        let state = Arc::new(Mutex::new(State::load(&paths)));

        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let (ui_tx, ui_rx) = channel();

        let worker = Worker {
            paths: paths.clone(),
            settings: shared_settings.clone(),
            state: state.clone(),
            log: log.clone(),
            tx: ev_tx,
            rx: cmd_rx,
            ctx: ctx.clone(),
            open: {
                let st = shared_settings.clone();
                Box::new(move || {
                    let p = st.lock().unwrap().port.clone();
                    crate::device::open_port(&p)
                })
            },
        };
        std::thread::Builder::new()
            .name("device-worker".into())
            .spawn(move || worker.run())
            .expect("spawn worker");

        let tray_tx = ui_tx.clone();
        let tray = Tray::new(ctx.clone(), cfg.language == "zh", move |c| {
            let _ = tray_tx.send(UiMsg::Tray(c));
        });
        if start_hidden && tray.is_none() {
            // Never leave the user without any way to reach the app.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        // Keep the registered autostart command valid if the portable folder moved.
        let mut autostart = platform::autostart_value().is_some();
        if autostart && !platform::autostart_points_here() {
            let _ = platform::autostart_set(true);
            autostart = platform::autostart_value().is_some();
        }

        log.info(format!(
            "ZNeo Manager {VERSION} started ({} mode, data in {})",
            if paths.portable { "portable" } else { "user" },
            paths.root.display()
        ));

        #[cfg(debug_assertions)]
        let demo = std::env::var("ZNEO_DEMO_DIR").ok();
        let mut app = App {
            paths,
            shared_settings,
            state,
            log,
            saved_cfg: cfg.clone(),
            upload_crop: cfg.crop,
            cfg,
            cmd_tx,
            ev_rx,
            ui_tx,
            ui_rx,
            tray,
            page: Page::Auto,
            status: DeviceStatus::default(),
            busy: None,
            progress: None,
            quitting: false,
            quit_confirm: false,
            show_pending: 0,
            upload_list: Vec::new(),
            selected: None,
            preview: None,
            preview_pending: None,
            preview_err: None,
            edit: ConfigEdit { brightness: 10, rotation: 0, screen_onoff_by_usb: false, switch_interval: 5, switch_random: false },
            edit_dirty: false,
            restore_confirm: None,
            autostart,
            autostart_err: None,
            folder_key: None,
            folder_count: None,
            ports: crate::device::list_ports(),
        };
        // Debug-build only: preload a folder to take UI screenshots.
        #[cfg(debug_assertions)]
        if let Some(d) = demo {
            if let Ok(list) = imaging::list_images(std::path::Path::new(&d), false) {
                app.add_files(list);
                app.page = Page::Upload;
            }
        }
        app
    }

    fn zh(&self) -> bool {
        self.cfg.language == "zh"
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }

    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.quit_confirm = true;
        } else {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.ev_rx.try_recv() {
            match ev {
                Event::Status(s) => {
                    if let (Some(c), false) = (&s.config, self.edit_dirty) {
                        self.edit = ConfigEdit {
                            brightness: c.brightness(),
                            rotation: c.rotation_degrees().unwrap_or(0),
                            screen_onoff_by_usb: c.screen_onoff_by_usb() != 0,
                            switch_interval: c.switch_interval(),
                            switch_random: c.switch_random() != 0,
                        };
                    }
                    self.status = s;
                }
                Event::Busy(b) => {
                    if b.is_none() {
                        self.progress = None;
                    }
                    self.busy = b;
                }
                Event::Progress(d, tot) => self.progress = Some((d, tot)),
                Event::ShowWindow => self.show_pending = 6,
            }
        }
        while let Ok(m) = self.ui_rx.try_recv() {
            match m {
                UiMsg::Tray(TrayCmd::Show) => self.show_pending = 6,
                UiMsg::Tray(TrayCmd::ChangeNow) => self.send(Cmd::ChangeNow),
                UiMsg::Tray(TrayCmd::Quit) => self.request_quit(ctx),
                UiMsg::FolderPicked(p) => self.cfg.folder = p.display().to_string(),
                UiMsg::FilesPicked(files) => self.add_files(files),
                UiMsg::RestorePicked(p) => self.restore_confirm = Some(p),
                UiMsg::Preview { path, crop, rgba } => {
                    self.preview_pending = None;
                    match rgba {
                        Ok(px) => {
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [DISPLAY_WIDTH as usize, DISPLAY_HEIGHT as usize],
                                &px,
                            );
                            let tex = ctx.load_texture("preview", img, egui::TextureOptions::LINEAR);
                            self.preview = Some((path, crop, tex));
                            self.preview_err = None;
                        }
                        Err(e) => {
                            self.preview = None;
                            self.preview_err = Some(e);
                        }
                    }
                }
                UiMsg::FolderCount { key, count } => {
                    if self.folder_key.as_ref() == Some(&key) {
                        self.folder_count = Some(count);
                    }
                }
            }
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn add_files(&mut self, files: Vec<PathBuf>) {
        for f in files {
            if imaging::is_image_path(&f) && !self.upload_list.contains(&f) {
                self.upload_list.push(f);
            }
        }
        if self.selected.is_none() && !self.upload_list.is_empty() {
            self.selected = Some(0);
        }
    }

    fn commit_settings(&mut self) {
        if self.cfg != self.saved_cfg {
            *self.shared_settings.lock().unwrap() = self.cfg.clone();
            if let Err(e) = self.cfg.save(&self.paths) {
                self.log.error(format!("Cannot save settings: {e:#}"));
            }
            self.saved_cfg = self.cfg.clone();
        }
    }

    fn ensure_preview(&mut self) {
        let Some(path) = self.selected.and_then(|i| self.upload_list.get(i)).cloned() else {
            self.preview = None;
            return;
        };
        let crop = self.upload_crop;
        let have = matches!(&self.preview, Some((p, c, _)) if *p == path && *c == crop);
        let pending = self.preview_pending.as_ref() == Some(&(path.clone(), crop));
        if have || pending {
            return;
        }
        self.preview_pending = Some((path.clone(), crop));
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let rgba = imaging::preview_rgba(&path, crop).map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::Preview { path, crop, rgba });
        });
    }

    fn ensure_folder_scan(&mut self) {
        let key = (self.cfg.folder.clone(), self.cfg.include_subfolders);
        if self.folder_key.as_ref() == Some(&key) {
            return;
        }
        self.folder_key = Some(key.clone());
        self.folder_count = None;
        if key.0.trim().is_empty() {
            return;
        }
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let count = imaging::list_images(std::path::Path::new(key.0.trim()), key.1)
                .map(|v| v.len())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::FolderCount { key, count });
        });
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut fonts = egui::FontDefinitions::default();
    if let Some(bytes) = platform::cjk_font_bytes() {
        fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&fam) {
                list.push("cjk".into());
            }
        }
    }
    ctx.set_fonts(fonts);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain(ctx);
        self.commit_settings();
        if self.show_pending > 0 {
            self.show_pending -= 1;
            self.show_window(ctx);
            // Keep the event loop turning until the window is really visible.
            ctx.request_repaint();
        }

        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            if self.cfg.minimize_to_tray && self.cfg.auto_enabled && self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else if self.busy.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit_confirm = true;
            }
        }
        // Keep the countdown / status text fresh without burning CPU.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let zh = self.zh();

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_files(dropped);
            self.page = Page::Upload;
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui, zh));
        egui::Panel::left("nav").exact_size(150.0).resizable(false).show(ui, |ui| {
            ui.add_space(6.0);
            let pages = [
                (Page::Auto, t(zh, "Auto change", "自動換圖")),
                (Page::Upload, t(zh, "Upload", "手動上傳")),
                (Page::Device, t(zh, "Device", "裝置設定")),
                (Page::Backup, t(zh, "Backup", "備份還原")),
                (Page::Log, t(zh, "Activity log", "活動記錄")),
                (Page::About, t(zh, "About", "關於")),
            ];
            for (p, label) in pages {
                if ui.selectable_label(self.page == p, RichText::new(label).size(15.0)).clicked() {
                    self.page = p;
                }
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.page {
                Page::Auto => self.page_auto(ui, zh),
                Page::Upload => self.page_upload(ui, zh),
                Page::Device => self.page_device(ui, zh),
                Page::Backup => self.page_backup(ui, zh),
                Page::Log => self.page_log(ui, zh),
                Page::About => self.page_about(ui, zh),
            });
        });

        self.dialogs(&ctx, zh);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.send(Cmd::Quit);
    }
}

// ------------------------------------------------------------------ pieces

impl App {
    fn header(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.horizontal(|ui| {
            let (color, text) = match (&self.status.info, &self.status.port) {
                (Some(info), Some(port)) => (
                    Color32::from_rgb(80, 200, 120),
                    format!(
                        "{}  {port} · FW {} · {} {}",
                        t(zh, "Connected", "已連線"),
                        info.fw_string(),
                        info.max_frames(),
                        t(zh, "frames max", "張上限")
                    ),
                ),
                _ => (
                    Color32::from_rgb(220, 90, 90),
                    match &self.status.error {
                        Some(e) if e.contains("in use by another program") => t(
                            zh,
                            "COM port is in use by another program (close hmlab.tech tabs / other HM software)",
                            "COM 埠被其他程式占用（請關閉瀏覽器的 hmlab.tech 分頁或其他 HM 軟體）",
                        )
                        .to_string(),
                        Some(e) if e.starts_with("no HM LAB device") => format!(
                            "{}",
                            t(zh, "Not connected: hub not found (check the USB cable)", "未連線：找不到 Hub（請確認 USB 已接上）")
                        ),
                        Some(e) => format!(
                            "{}: {} {}",
                            t(zh, "Not connected", "未連線"),
                            first_line(e),
                            t(zh, "(see Device tab)", "（可到「裝置設定」選連接埠）")
                        ),
                        None => t(zh, "Not connected", "未連線").to_string(),
                    },
                ),
            };
            ui.label(RichText::new("●").color(color).size(16.0));
            ui.label(text);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(self.busy.is_none(), egui::Button::new(t(zh, "Refresh", "重新整理"))).clicked() {
                    self.send(Cmd::Refresh);
                }
                if let Some(label) = &self.busy {
                    ui.spinner();
                    if let Some((d, tot)) = self.progress {
                        if tot > 0 {
                            ui.add(egui::ProgressBar::new(d as f32 / tot as f32).desired_width(140.0).show_percentage());
                        }
                    }
                    ui.label(label.as_str());
                }
            });
        });
    }

    fn page_auto(&mut self, ui: &mut egui::Ui, zh: bool) {
        self.ensure_folder_scan();
        ui.heading(t(zh, "Automatic picture change", "自動換圖"));
        ui.label(t(
            zh,
            "While this app is running (it can live in the system tray), it uploads the next picture from your folder to the hub at the interval you choose.",
            "程式執行期間（可縮小到系統匣常駐），會依設定的間隔，從你選的資料夾挑下一張圖上傳到 Hub 螢幕。",
        ));
        ui.separator();

        ui.checkbox(&mut self.cfg.auto_enabled, t(zh, "Enable automatic change", "啟用自動換圖"));

        ui.horizontal(|ui| {
            ui.label(t(zh, "Folder:", "圖片資料夾："));
            ui.add(egui::TextEdit::singleline(&mut self.cfg.folder).desired_width(360.0));
            if ui.button(t(zh, "Browse…", "選擇…")).clicked() {
                let tx = self.ui_tx.clone();
                let title = t(zh, "Choose picture folder", "選擇圖片資料夾");
                std::thread::spawn(move || {
                    if let Some(p) = platform::pick_folder(title) {
                        let _ = tx.send(UiMsg::FolderPicked(p));
                    }
                });
            }
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.include_subfolders, t(zh, "Include subfolders", "包含子資料夾"));
            match &self.folder_count {
                Some(Ok(n)) => {
                    ui.label(format!("{n} {}", t(zh, "images found", "張圖片")));
                }
                Some(Err(e)) => {
                    ui.colored_label(Color32::from_rgb(230, 120, 100), first_line(e));
                }
                None if !self.cfg.folder.trim().is_empty() => {
                    ui.label(t(zh, "scanning…", "掃描中…"));
                }
                None => {}
            }
        });

        ui.horizontal(|ui| {
            ui.label(t(zh, "Change every:", "更換間隔："));
            let mut hours = self.cfg.interval_minutes as f64 / 60.0;
            if ui
                .add(egui::DragValue::new(&mut hours).range(0.1..=168.0).speed(0.05).max_decimals(2).suffix(t(zh, " h", " 小時")))
                .changed()
            {
                self.cfg.interval_minutes = ((hours * 60.0).round() as u32).max(5);
            }
            for (label, mins) in [("30m", 30), ("1h", 60), ("2h", 120), ("6h", 360), ("12h", 720), ("24h", 1440)] {
                if ui.selectable_label(self.cfg.interval_minutes == mins, label).clicked() {
                    self.cfg.interval_minutes = mins;
                }
            }
        });
        if self.cfg.interval_minutes < 10 {
            ui.colored_label(
                Color32::from_rgb(230, 180, 80),
                t(
                    zh,
                    "Very short intervals rewrite the device flash often. 10 minutes or more is recommended.",
                    "間隔太短會頻繁覆寫裝置快閃記憶體，建議 10 分鐘以上。",
                ),
            );
        }

        ui.horizontal(|ui| {
            ui.label(t(zh, "Order:", "順序："));
            ui.selectable_value(&mut self.cfg.order, Order::Sequential, t(zh, "In order", "依序"));
            ui.selectable_value(&mut self.cfg.order, Order::Random, t(zh, "Random", "隨機"));
        });
        ui.horizontal(|ui| {
            ui.label(t(zh, "Pictures per change:", "每次上傳張數："));
            ui.add(egui::DragValue::new(&mut self.cfg.images_per_change).range(1..=20));
            ui.label(t(
                zh,
                "(more than 1: the hub cycles them itself, see Device tab)",
                "（大於 1 時由 Hub 自己輪播，間隔在「裝置設定」）",
            ));
        });
        ui.horizontal(|ui| {
            ui.label(t(zh, "Fit:", "縮放方式："));
            ui.selectable_value(&mut self.cfg.crop, true, t(zh, "Fill (crop)", "填滿（裁切）"));
            ui.selectable_value(&mut self.cfg.crop, false, t(zh, "Fit (black bars)", "完整顯示（黑邊）"));
        });
        ui.horizontal(|ui| {
            ui.label(t(zh, "GIF over the frame limit:", "GIF 超過幀數上限時："));
            ui.selectable_value(&mut self.cfg.gif_overflow, crate::imaging::GifOverflow::Trim, t(zh, "Cut the tail", "截掉後面的幀"));
            ui.selectable_value(&mut self.cfg.gif_overflow, crate::imaging::GifOverflow::Sample, t(zh, "Sample evenly (keeps whole loop)", "均勻抽幀（保留完整動畫）"));
        });
        ui.checkbox(
            &mut self.cfg.change_on_launch,
            t(zh, "Change picture right when the app starts", "程式啟動時立即換一張"),
        );

        ui.separator();
        let before = self.autostart;
        ui.checkbox(&mut self.autostart, t(zh, "Start with Windows (minimized to tray)", "開機自動啟動（縮小到系統匣）"));
        if self.autostart != before {
            match platform::autostart_set(self.autostart) {
                Ok(()) => self.autostart_err = None,
                Err(e) => {
                    self.autostart_err = Some(e);
                    self.autostart = before;
                }
            }
        }
        if let Some(e) = &self.autostart_err {
            ui.colored_label(Color32::from_rgb(230, 120, 100), e.as_str());
        }
        ui.checkbox(
            &mut self.cfg.minimize_to_tray,
            t(zh, "When automatic change is on, the close button hides to the tray (otherwise it quits)", "自動換圖開啟時，按關閉鈕縮到系統匣常駐（未開啟時直接結束）"),
        );
        ui.horizontal(|ui| {
            ui.label(t(zh, "Language:", "語言："));
            ui.selectable_value(&mut self.cfg.language, "zh".to_string(), "繁體中文");
            ui.selectable_value(&mut self.cfg.language, "en".to_string(), "English");
        });

        ui.separator();
        let st = self.state.lock().unwrap().clone();
        let fmt = |ts: u64| {
            chrono::DateTime::from_timestamp(ts as i64, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        };
        if st.last_change > 0 {
            ui.label(format!("{} {}", t(zh, "Last change:", "上次換圖："), fmt(st.last_change)));
            if !st.last_files.is_empty() {
                ui.label(format!("   {}", st.last_files.join(", ")));
            }
        } else {
            ui.label(t(zh, "No automatic change has run yet.", "尚未執行過自動換圖。"));
        }
        let next = if !self.cfg.auto_enabled {
            t(zh, "Automatic change is off.", "自動換圖未啟用。").to_string()
        } else if self.cfg.folder.trim().is_empty() {
            t(zh, "Choose a folder first.", "請先選擇資料夾。").to_string()
        } else if self.status.port.is_none() {
            t(zh, "Waiting for the hub to be connected…", "等待 Hub 連線…").to_string()
        } else {
            let due = schedule::next_due(st.last_change, self.cfg.interval_minutes);
            if schedule::now_unix() >= due {
                t(zh, "Next change: now", "下次換圖：即將執行").to_string()
            } else {
                format!("{} {}", t(zh, "Next change:", "下次換圖："), fmt(due))
            }
        };
        ui.label(next);
        ui.horizontal(|ui| {
            let ok = self.busy.is_none() && !self.cfg.folder.trim().is_empty() && self.status.port.is_some();
            if ui.add_enabled(ok, egui::Button::new(t(zh, "Change now", "立即換圖"))).clicked() {
                self.send(Cmd::ChangeNow);
            }
            if ui.button(t(zh, "Open folder", "開啟資料夾")).clicked() && !self.cfg.folder.trim().is_empty() {
                platform::open_in_explorer(std::path::Path::new(self.cfg.folder.trim()));
            }
        });
        ui.add_space(4.0);
        ui.weak(t(
            zh,
            "Note: each change replaces everything stored on the hub with the newly picked picture(s).",
            "注意：每次換圖都會用新挑選的圖片取代 Hub 內原有的所有圖片。",
        ));
    }

    fn page_upload(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.heading(t(zh, "Manual upload", "手動上傳"));
        ui.weak(t(
            zh,
            "Drag image files here or use Add. All listed files are uploaded together and replace what is on the hub.",
            "把圖片拖進視窗，或按「加入」。清單中的所有檔案會一起上傳，並取代 Hub 內現有內容。",
        ));
        ui.horizontal(|ui| {
            if ui.button(t(zh, "Add images…", "加入圖片…")).clicked() {
                let tx = self.ui_tx.clone();
                let title = t(zh, "Choose images", "選擇圖片");
                std::thread::spawn(move || {
                    let files = platform::pick_files(title);
                    if !files.is_empty() {
                        let _ = tx.send(UiMsg::FilesPicked(files));
                    }
                });
            }
            if ui.button(t(zh, "Clear list", "清空清單")).clicked() {
                self.upload_list.clear();
                self.selected = None;
            }
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.upload_crop, true, t(zh, "Fill (crop)", "填滿（裁切）"));
            ui.selectable_value(&mut self.upload_crop, false, t(zh, "Fit (black bars)", "完整顯示（黑邊）"));
        });
        ui.horizontal(|ui| {
            ui.label(t(zh, "GIF over the frame limit:", "GIF 超過幀數上限時："));
            ui.selectable_value(&mut self.cfg.gif_overflow, crate::imaging::GifOverflow::Trim, t(zh, "Cut the tail", "截掉後面的幀"));
            ui.selectable_value(&mut self.cfg.gif_overflow, crate::imaging::GifOverflow::Sample, t(zh, "Sample evenly (keeps whole loop)", "均勻抽幀（保留完整動畫）"));
        });
        self.ensure_preview();

        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(220.0);
                let mut remove = None;
                for (i, p) in self.upload_list.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.selectable_label(self.selected == Some(i), file_name(p)).clicked() {
                            self.selected = Some(i);
                        }
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.upload_list.remove(i);
                    self.selected = match self.upload_list.len() {
                        0 => None,
                        n => Some(i.min(n - 1)),
                    };
                }
            });
            ui.vertical(|ui| {
                let size = Vec2::new(DISPLAY_WIDTH as f32 * 1.3, DISPLAY_HEIGHT as f32 * 1.3);
                match &self.preview {
                    Some((_, _, tex)) => {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                    }
                    None => {
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter().rect_filled(rect, 4.0, Color32::from_gray(20));
                        let msg = match (&self.preview_err, self.preview_pending.is_some()) {
                            (Some(e), _) => first_line(e),
                            (None, true) => t(zh, "Rendering…", "產生預覽…").to_string(),
                            _ => t(zh, "No image selected", "尚未選擇圖片").to_string(),
                        };
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(14.0), Color32::GRAY);
                    }
                }
                ui.weak(format!(
                    "{}: {}×{} ({})",
                    t(zh, "Device screen", "裝置螢幕"),
                    DISPLAY_WIDTH,
                    DISPLAY_HEIGHT,
                    t(zh, "preview shows exactly what is sent", "預覽即實際送出的內容")
                ));
            });
        });

        ui.separator();
        let ok = self.busy.is_none() && !self.upload_list.is_empty() && self.status.port.is_some();
        if ui.add_enabled(ok, egui::Button::new(RichText::new(t(zh, "Upload to device", "上傳到裝置")).size(15.0))).clicked() {
            self.send(Cmd::Upload { paths: self.upload_list.clone(), crop: self.upload_crop });
        }
        if self.status.port.is_none() {
            ui.weak(t(zh, "Connect the hub first.", "請先連接 Hub。"));
        }
    }

    fn page_device(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.heading(t(zh, "Device", "裝置設定"));
        ui.horizontal(|ui| {
            ui.label(t(zh, "COM port:", "COM 連接埠："));
            let current = if self.cfg.port.is_empty() { t(zh, "Auto-detect", "自動偵測").to_string() } else { self.cfg.port.clone() };
            egui::ComboBox::from_id_salt("port").selected_text(current).width(300.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.port, String::new(), t(zh, "Auto-detect", "自動偵測"));
                for (name, desc) in &self.ports {
                    ui.selectable_value(&mut self.cfg.port, name.clone(), format!("{name}  {desc}"));
                }
            });
            if ui.button(t(zh, "Rescan ports", "重新掃描")).clicked() {
                self.ports = crate::device::list_ports();
            }
            if ui.add_enabled(self.busy.is_none(), egui::Button::new(t(zh, "Connect", "重新連線"))).clicked() {
                self.commit_settings();
                self.send(Cmd::Refresh);
            }
        });
        if ui.add_enabled(self.busy.is_none(), egui::Button::new(t(zh, "Run connection diagnostics (~25 s)", "執行連線診斷（約 25 秒）"))).clicked() {
            self.commit_settings();
            self.send(Cmd::Diagnose);
            self.page = Page::Log;
        }
        ui.weak(t(
            zh,
            "Auto-detect looks for USB ID C019:0401. If the hub is not answering, try another port from the list.",
            "自動偵測是找 USB ID C019:0401。若 Hub 沒有回應，可從清單改選其他連接埠。",
        ));
        ui.separator();
        let Some(info) = self.status.info.clone() else {
            ui.label(t(zh, "No device connected.", "尚未連線到裝置。"));
            return;
        };
        egui::Grid::new("info").num_columns(2).show(ui, |ui| {
            ui.label(t(zh, "Port", "連接埠"));
            ui.label(self.status.port.clone().unwrap_or_default());
            ui.end_row();
            ui.label(t(zh, "Hardware ID", "硬體 ID"));
            ui.label(format!("{:#010x}", info.hw_id));
            ui.end_row();
            ui.label(t(zh, "Firmware", "韌體"));
            ui.label(info.fw_string());
            ui.end_row();
            ui.label(t(zh, "Flash / max frames", "Flash／影格上限"));
            ui.label(format!("{} MB / {}", info.flash_size / 1024 / 1024, info.max_frames()));
            ui.end_row();
            if let Some(p) = &self.status.power {
                ui.label(t(zh, "USB power", "USB 電力"));
                ui.label(format!(
                    "{:.2} V · {} / {} / {} mA",
                    p.bus_mv as f32 / 1000.0,
                    p.port_ma[0],
                    p.port_ma[1],
                    p.port_ma[2]
                ));
                ui.end_row();
            }
        });
        ui.separator();
        if self.status.config.is_none() {
            ui.label(t(zh, "Could not read device settings.", "無法讀取裝置設定。"));
            return;
        }
        let mut d = false;
        ui.horizontal(|ui| {
            ui.label(t(zh, "Brightness", "亮度"));
            d |= ui.add(egui::Slider::new(&mut self.edit.brightness, 0..=30)).changed();
        });
        ui.horizontal(|ui| {
            ui.label(t(zh, "Rotation", "旋轉"));
            for deg in [0u16, 90, 180, 270] {
                d |= ui.selectable_value(&mut self.edit.rotation, deg, format!("{deg}°")).changed();
            }
        });
        d |= ui.checkbox(&mut self.edit.screen_onoff_by_usb, t(zh, "Screen on/off follows USB", "螢幕隨 USB 開關")).changed();
        ui.horizontal(|ui| {
            ui.label(t(zh, "Hub-side switch interval (s)", "Hub 輪播間隔（秒）"));
            d |= ui.add(egui::Slider::new(&mut self.edit.switch_interval, 0..=255)).changed();
        });
        d |= ui.checkbox(&mut self.edit.switch_random, t(zh, "Hub-side random order", "Hub 輪播隨機順序")).changed();
        if d {
            self.edit_dirty = true;
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(self.busy.is_none(), egui::Button::new(t(zh, "Apply & verify", "套用並驗證"))).clicked() {
                self.send(Cmd::ApplyConfig(self.edit));
                self.edit_dirty = false;
            }
            if ui.add_enabled(self.edit_dirty, egui::Button::new(t(zh, "Revert", "還原"))).clicked() {
                self.edit_dirty = false;
                self.send(Cmd::Refresh);
            }
        });
        ui.weak(t(
            zh,
            "Settings are read back from the device after applying; failures are reported in the log.",
            "套用後會從裝置讀回比對，失敗會記錄在活動記錄。",
        ));
    }

    fn page_backup(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.heading(t(zh, "Backup & restore", "備份還原"));
        ui.weak(t(
            zh,
            "A backup contains the device settings and every stored picture. Restoring overwrites both.",
            "備份包含裝置設定與所有已儲存圖片；還原會同時覆寫兩者。",
        ));
        let dir = self.paths.backups_dir();
        let ready = self.busy.is_none() && self.status.port.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new(t(zh, "Create backup", "建立備份"))).clicked() {
                let name = chrono::Local::now().format("zneo-%Y%m%d-%H%M%S.bak").to_string();
                self.send(Cmd::Backup(dir.join(name)));
            }
            if ui.add_enabled(ready, egui::Button::new(t(zh, "Restore from file…", "從檔案還原…"))).clicked() {
                let tx = self.ui_tx.clone();
                let title = t(zh, "Choose a backup", "選擇備份檔");
                std::thread::spawn(move || {
                    if let Some(p) = platform::open_file(title) {
                        let _ = tx.send(UiMsg::RestorePicked(p));
                    }
                });
            }
            if ui.button(t(zh, "Open folder", "開啟資料夾")).clicked() {
                platform::open_in_explorer(&dir);
            }
        });
        ui.separator();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "bak")).collect())
            .unwrap_or_default();
        files.sort();
        files.reverse();
        if files.is_empty() {
            ui.label(t(zh, "No backups yet.", "尚無備份。"));
        }
        for p in files {
            ui.horizontal(|ui| {
                let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                ui.label(format!("{}  ({:.1} MB)", file_name(&p), size as f64 / 1_048_576.0));
                if ui.add_enabled(ready, egui::Button::new(t(zh, "Restore", "還原"))).clicked() {
                    self.restore_confirm = Some(p.clone());
                }
            });
        }
    }

    fn page_log(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.heading(t(zh, "Activity log", "活動記錄"));
        if ui.button(t(zh, "Open log folder", "開啟記錄資料夾")).clicked() {
            if let Some(d) = self.paths.log_file().parent() {
                platform::open_in_explorer(d);
            }
        }
        ui.separator();
        for e in self.log.entries().iter().rev() {
            let color = match e.level {
                Level::Info => Color32::from_gray(200),
                Level::Warn => Color32::from_rgb(230, 180, 80),
                Level::Error => Color32::from_rgb(230, 110, 100),
            };
            ui.label(RichText::new(format!("{}  {}", e.time, e.msg)).color(color).monospace());
        }
    }

    fn page_about(&mut self, ui: &mut egui::Ui, zh: bool) {
        ui.heading(format!("Z-NEO Manager {VERSION}"));
        ui.label(t(
            zh,
            "Independent local manager for the HM LAB Z-NEO 8K USB hub LCD. No cloud, no telemetry.",
            "HM LAB Z-NEO 8K USB Hub 螢幕的本機管理程式。不需雲端、不傳送任何資料。",
        ));
        ui.label(format!(
            "{}: {} ({})",
            t(zh, "Data folder", "資料位置"),
            self.paths.root.display(),
            if self.paths.portable { t(zh, "portable", "可攜模式") } else { t(zh, "per-user fallback", "使用者資料夾（程式資料夾不可寫）") }
        ));
        ui.separator();
        ui.label("Device protocol derived from the open-source hm-hub project (Creationsss/hm-hub), AGPL-3.0-or-later.");
        ui.label("This program is licensed AGPL-3.0-or-later; source code must accompany any distributed binary.");
        ui.weak(t(
            zh,
            "Not affiliated with HM LAB. Protocol behaviour may differ between firmware versions.",
            "與 HM LAB 無隸屬關係。不同韌體版本的協定行為可能不同。",
        ));
    }

    fn dialogs(&mut self, ctx: &egui::Context, zh: bool) {
        if let Some(path) = self.restore_confirm.clone() {
            egui::Window::new(t(zh, "Confirm restore", "確認還原"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(format!("{}\n{}", file_name(&path), t(zh, "This overwrites the device settings and all pictures.", "這會覆寫裝置設定與所有圖片。")));
                    ui.horizontal(|ui| {
                        if ui.button(t(zh, "Restore", "還原")).clicked() {
                            self.send(Cmd::Restore(path.clone()));
                            self.restore_confirm = None;
                        }
                        if ui.button(t(zh, "Cancel", "取消")).clicked() {
                            self.restore_confirm = None;
                        }
                    });
                });
        }
        if self.quit_confirm {
            egui::Window::new(t(zh, "Operation in progress", "作業進行中"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(t(
                        zh,
                        "The device is being written. Quitting now may leave it with incomplete data.",
                        "裝置正在寫入中，現在結束可能讓裝置內容不完整。",
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(t(zh, "Wait", "繼續等待")).clicked() {
                            self.quit_confirm = false;
                        }
                        if ui.button(t(zh, "Quit anyway", "仍要結束")).clicked() {
                            self.quitting = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
