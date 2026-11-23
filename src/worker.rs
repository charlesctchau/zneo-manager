// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The device protocol (packet layout, flash layout, config block) is derived from
// hm-hub by Creationsss (https://github.com/Creationsss/hm-hub),
// licensed AGPL-3.0-or-later.
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! Background thread that owns every device operation (strictly serialized)
//! and runs the automatic picture-rotation schedule.

use anyhow::{bail, Context, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::device::{self, DynDevice};
use crate::imaging;
use crate::proto::*;
use crate::schedule::{self, Rng};
use crate::settings::{Logger, Paths, Settings, State};

pub enum Cmd {
    Refresh,
    Upload { paths: Vec<PathBuf>, crop: bool },
    ChangeNow,
    ApplyConfig(ConfigEdit),
    Diagnose,
    Backup(PathBuf),
    Restore(PathBuf),
    Quit,
}

#[derive(Clone, Copy)]
pub struct ConfigEdit {
    pub brightness: u8,
    pub rotation: u16,
    pub screen_onoff_by_usb: bool,
    pub switch_interval: u8,
    pub switch_random: bool,
}

#[derive(Default, Clone)]
pub struct DeviceStatus {
    pub port: Option<String>,
    pub info: Option<DeviceInfo>,
    pub config: Option<DeviceConfig>,
    pub power: Option<PowerStats>,
    pub error: Option<String>,
}

pub enum Event {
    Status(DeviceStatus),
    /// Label of the running operation, or None when idle.
    Busy(Option<String>),
    Progress(u64, u64),
    ShowWindow,
}

pub struct Worker {
    pub paths: Paths,
    pub settings: Arc<Mutex<Settings>>,
    pub state: Arc<Mutex<State>>,
    pub log: Arc<Logger>,
    pub tx: Sender<Event>,
    pub rx: Receiver<Cmd>,
    pub ctx: egui::Context,
    /// Opens the device (serial port in production, a mock in tests).
    pub open: Box<dyn Fn() -> Result<(String, DynDevice)> + Send>,
}

impl Worker {
    fn emit(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    pub fn run(self) {
        let mut rng = Rng::new();
        let mut last_port: Option<String> = None;
        let mut last_scan_err = String::new();
        let mut last_scan = std::time::Instant::now() - Duration::from_secs(60);
        let mut launch_checked = false;
        let mut force_due = false;
        let mut retry_after: u64 = 0;
        let mut rotate_err_logged = String::new();

        loop {
            match self.rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Cmd::Refresh) => {
                    last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                }
                Ok(Cmd::Upload { paths, crop }) => {
                    self.op("Upload", |w| w.manual_upload(&paths, crop));
                    last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                }
                Ok(Cmd::ChangeNow) => {
                    if self.op("Change picture", |w| w.rotate(&mut rng)).is_some() {
                        retry_after = 0;
                    }
                    last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                }
                Ok(Cmd::ApplyConfig(edit)) => {
                    self.op("Apply settings", |w| w.apply_config(edit));
                    last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                }
                Ok(Cmd::Diagnose) => {
                    self.op("Diagnostics", |w| {
                        let ov = w.settings.lock().unwrap().port.clone();
                        let port = device::resolve_port(&ov)?;
                        for line in device::diagnose(&port) {
                            w.log.info(line);
                        }
                        w.log.info("Diagnostics finished");
                        Ok(())
                    });
                    last_port = None;
                    last_scan = std::time::Instant::now() - Duration::from_secs(60);
                }
                Ok(Cmd::Backup(path)) => {
                    self.op("Backup", |w| w.backup(&path));
                }
                Ok(Cmd::Restore(path)) => {
                    self.op("Restore", |w| w.restore(&path));
                    last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            // A second launched instance asks this one to show its window.
            if std::fs::remove_file(self.paths.show_request_file()).is_ok() {
                self.emit(Event::ShowWindow);
            }

            // Cheap hot-plug detection: enumeration only, no device I/O.
            if last_scan.elapsed() >= Duration::from_secs(5) {
                last_scan = std::time::Instant::now();
                let ov = self.settings.lock().unwrap().port.clone();
                match device::resolve_port(&ov) {
                    Ok(port) => {
                        if last_port.as_deref() != Some(port.as_str()) {
                            let st = self.op("Refresh", |w| w.refresh());
                            match st {
                                Some(s) if s.port.is_some() => {
                                    last_port = s.port;
                                    last_scan_err.clear();
                                }
                                Some(s) => {
                                    let e = s.error.unwrap_or_default();
                                    if e != last_scan_err {
                                        self.log.warn(format!("Device detected on {port} but query failed: {e}"));
                                        last_scan_err = e;
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                    Err(e) => {
                        if last_port.is_some() || last_scan_err != "none" {
                            if last_port.is_some() {
                                self.log.warn("Device disconnected");
                            }
                            last_port = None;
                            last_scan_err = "none".into();
                            self.emit(Event::Status(DeviceStatus { error: Some(e.to_string()), ..Default::default() }));
                        }
                    }
                }
            }

            // Automatic rotation.
            let s = self.settings.lock().unwrap().clone();
            if s.auto_enabled && !s.folder.trim().is_empty() {
                if !launch_checked {
                    launch_checked = true;
                    force_due = s.change_on_launch;
                }
                let now = schedule::now_unix();
                let last = self.state.lock().unwrap().last_change;
                let due = force_due || schedule::is_due(now, last, s.interval_minutes);
                if due && now >= retry_after && last_port.is_some() {
                    match self.op("Auto change", |w| w.rotate(&mut rng)) {
                        Some(()) => {
                            force_due = false;
                            rotate_err_logged.clear();
                            last_port = self.op("Refresh", |w| w.refresh()).and_then(|s| s.port);
                        }
                        None => retry_after = now + 60,
                    }
                }
            }
        }
    }

    /// Run one operation with Busy events and uniform error logging.
    fn op<R>(&self, label: &str, f: impl FnOnce(&Worker) -> Result<R>) -> Option<R> {
        self.emit(Event::Busy(Some(label.to_string())));
        let r = f(self);
        self.emit(Event::Busy(None));
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                // Refresh failures are reported through DeviceStatus, not as log noise.
                if label != "Refresh" {
                    self.log.error(format!("{label} failed: {e:#}"));
                }
                None
            }
        }
    }

    fn progress_cb(&self) -> impl FnMut(u64, u64) + '_ {
        let mut last_pct = u64::MAX;
        move |done, total| {
            let pct = if total == 0 { 100 } else { done * 100 / total };
            if pct != last_pct {
                last_pct = pct;
                self.emit(Event::Progress(done, total));
            }
        }
    }

    /// Query the device and publish a fresh status. Returns the status (Ok even on
    /// a failed query, so the UI can show the reason).
    fn refresh(&self) -> Result<DeviceStatus> {
        let status = match (self.open)() {
            Ok((port, mut dev)) => {
                let config = dev.read_config().ok();
                let power = dev.read_power(Duration::from_secs(3)).ok();
                DeviceStatus { port: Some(port), info: Some(dev.info.clone()), config, power, error: None }
            }
            Err(e) => DeviceStatus { error: Some(format!("{e:#}")), ..Default::default() },
        };
        self.emit(Event::Status(status.clone()));
        Ok(status)
    }

    fn max_frames_probe(&self) -> Result<usize> {
        let (_, dev) = (self.open)()?;
        Ok(dev.info.max_frames())
    }

    fn manual_upload(&self, paths: &[PathBuf], crop: bool) -> Result<()> {
        if paths.is_empty() {
            bail!("no files selected");
        }
        let max = self.max_frames_probe()?;
        // Decode everything first: an invalid file aborts before any device write.
        let mut albums = Vec::new();
        for p in paths {
            let a = imaging::load_album(p, crop, max)
                .with_context(|| format!("{}", p.display()))?;
            albums.push(a);
        }
        let total: usize = albums.iter().map(|a| a.frames.len()).sum();
        let (port, mut dev) = (self.open)()?;
        let image = build_flash_buffer(&albums, dev.info.flash_size)?;
        self.log.info(format!("Uploading {} file(s), {total} frame(s) via {port}", paths.len()));
        let t0 = std::time::Instant::now();
        let mut cb = self.progress_cb();
        dev.upload_flash(&image, &mut cb)?;
        self.log.info(format!(
            "Upload finished in {:.1}s: {}",
            t0.elapsed().as_secs_f32(),
            paths.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")
        ));
        Ok(())
    }

    /// Pick the next image(s) from the configured folder and upload them.
    fn rotate(&self, rng: &mut Rng) -> Result<()> {
        let s = self.settings.lock().unwrap().clone();
        let folder = PathBuf::from(s.folder.trim());
        if s.folder.trim().is_empty() {
            bail!("no folder selected");
        }
        let files = imaging::list_images(&folder, s.include_subfolders)?;
        if files.is_empty() {
            bail!("no supported images in {}", folder.display());
        }
        let max = self.max_frames_probe()?;
        let (cursor, last_files) = {
            let st = self.state.lock().unwrap();
            (st.cursor, st.last_files.clone())
        };
        let avoid = last_files
            .first()
            .and_then(|n| files.iter().position(|f| file_name(f) == *n));
        let order = schedule::candidate_order(files.len(), s.order, cursor, rng, avoid);

        let want = s.images_per_change.clamp(1, 50) as usize;
        let mut albums = Vec::new();
        let mut names = Vec::new();
        let mut frames_used = 0usize;
        let mut consumed = 0usize;
        for idx in order {
            consumed += 1;
            let path = &files[idx];
            match imaging::load_album(path, s.crop, max) {
                Ok(a) if frames_used + a.frames.len() <= max => {
                    frames_used += a.frames.len();
                    names.push(file_name(path));
                    albums.push(a);
                }
                Ok(_) => self.log.warn(format!("Skipped {} (does not fit in remaining frames)", file_name(path))),
                Err(e) => self.log.warn(format!("Skipped {}: {e:#}", file_name(path))),
            }
            if albums.len() >= want {
                break;
            }
        }
        if albums.is_empty() {
            bail!("none of the images in the folder could be used");
        }

        let (port, mut dev) = (self.open)()?;
        let image = build_flash_buffer(&albums, dev.info.flash_size)?;
        let t0 = std::time::Instant::now();
        let mut cb = self.progress_cb();
        dev.upload_flash(&image, &mut cb)?;
        self.log.info(format!(
            "Picture changed via {port} in {:.1}s: {}",
            t0.elapsed().as_secs_f32(),
            names.join(", ")
        ));

        let mut st = self.state.lock().unwrap();
        st.last_change = schedule::now_unix();
        st.cursor = (cursor + consumed) % files.len();
        st.last_files = names;
        if let Err(e) = st.save(&self.paths) {
            self.log.warn(format!("Could not save state: {e:#}"));
        }
        Ok(())
    }

    fn apply_config(&self, edit: ConfigEdit) -> Result<()> {
        let (_, mut dev) = (self.open)()?;
        let mut cfg = dev.read_config()?;
        cfg.set_brightness(edit.brightness)?;
        cfg.set_rotation_degrees(edit.rotation)?;
        cfg.set_screen_onoff_by_usb(edit.screen_onoff_by_usb);
        cfg.set_switch_interval(edit.switch_interval);
        cfg.set_switch_random(edit.switch_random);
        let back = dev.write_config_verified(&cfg)?;
        self.log.info(format!("Device settings applied and verified: {back}"));
        Ok(())
    }

    fn backup(&self, path: &std::path::Path) -> Result<()> {
        let (_, mut dev) = (self.open)()?;
        let cfg = dev.read_config()?.to_bytes();
        let mut cb = self.progress_cb();
        let flash = dev.read_flash(&mut cb)?;
        let file = encode_backup(&cfg, &flash);
        std::fs::write(path, &file).with_context(|| format!("cannot write {}", path.display()))?;
        self.log.info(format!("Backup saved: {} ({:.1} MB)", path.display(), file.len() as f64 / 1_048_576.0));
        Ok(())
    }

    fn restore(&self, path: &std::path::Path) -> Result<()> {
        let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let (cfg_bytes, flash) = decode_backup(&data)?;
        let cfg = DeviceConfig::from_bytes(&cfg_bytes)?;
        let (_, mut dev) = (self.open)()?;
        if flash.len() as u32 > dev.info.flash_size {
            bail!("backup is larger than this device's flash");
        }
        let mut cb = self.progress_cb();
        dev.upload_flash(&flash, &mut cb)?;
        dev.write_config_verified(&cfg)?;
        self.log.info(format!("Restore complete from {}", path.display()));
        Ok(())
    }
}

pub fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

// Same container format as hm-hub: "HMHUB" 0x01, cfg len+cfg, flash len+flash, CRC32.
pub fn encode_backup(cfg: &[u8], flash: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(flash.len() + 64);
    b.extend_from_slice(b"HMHUB\x01");
    b.extend_from_slice(&(cfg.len() as u32).to_le_bytes());
    b.extend_from_slice(cfg);
    b.extend_from_slice(&(flash.len() as u32).to_le_bytes());
    b.extend_from_slice(flash);
    let crc = crc32fast::hash(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

pub fn decode_backup(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    if data.len() < 18 || &data[..5] != b"HMHUB" {
        bail!("not a valid backup file");
    }
    if data[5] != 1 {
        bail!("unsupported backup version {}", data[5]);
    }
    let (body, tail) = data.split_at(data.len() - 4);
    if crc32fast::hash(body) != u32::from_le_bytes(tail.try_into().unwrap()) {
        bail!("backup file is corrupted (CRC mismatch)");
    }
    let mut pos = 6usize;
    let take_block = |pos: &mut usize| -> Result<Vec<u8>> {
        let len = body
            .get(*pos..*pos + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .context("truncated backup")?;
        *pos += 4;
        let block = body.get(*pos..*pos + len).context("truncated backup")?.to_vec();
        *pos += len;
        Ok(block)
    };
    let cfg = take_block(&mut pos)?;
    let flash = take_block(&mut pos)?;
    Ok((cfg, flash))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mock::MockDevice;
    use crate::device::{Device, Transport};
    use crate::schedule::Order;
    use image::{Rgba, RgbaImage};
    use std::sync::mpsc::channel;

    #[test]
    fn backup_roundtrip_and_corruption() {
        let cfg = [1u8; 24];
        let flash = vec![9u8; 5000];
        let enc = encode_backup(&cfg, &flash);
        let (c, f) = decode_backup(&enc).unwrap();
        assert_eq!(c, cfg);
        assert_eq!(f, flash);
        let mut bad = enc.clone();
        bad[30] ^= 1;
        assert!(decode_backup(&bad).is_err());
        assert!(decode_backup(&enc[..10]).is_err());
        assert!(decode_backup(b"garbage garbage garbage").is_err());
    }

    /// Build a worker wired to a mock device; returns it with the shared flash buffer.
    fn rig(name: &str, order: Order, per_change: u32, colors: &[[u8; 3]]) -> (Worker, Arc<Mutex<Vec<u8>>>, PathBuf) {
        let root = std::env::temp_dir().join(format!("zneo_rig_{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let pics = root.join("pics");
        std::fs::create_dir_all(&pics).unwrap();
        for (i, c) in colors.iter().enumerate() {
            RgbaImage::from_pixel(80, 40, Rgba([c[0], c[1], c[2], 255]))
                .save(pics.join(format!("{i}.png")))
                .unwrap();
        }
        std::fs::write(pics.join("broken.png"), b"junk").unwrap();
        let paths = Paths { root: root.clone(), portable: true };
        paths.ensure_dirs().unwrap();
        let settings = Settings {
            auto_enabled: true,
            folder: pics.display().to_string(),
            order,
            images_per_change: per_change,
            ..Settings::default()
        };
        let flash: Arc<Mutex<Vec<u8>>> = Default::default();
        let f2 = flash.clone();
        let (tx, _ev_rx) = channel();
        let (_cmd_tx, rx) = channel();
        let w = Worker {
            paths: paths.clone(),
            settings: Arc::new(Mutex::new(settings)),
            state: Arc::new(Mutex::new(State::default())),
            log: Arc::new(Logger::new(paths.log_file())),
            tx,
            rx,
            ctx: egui::Context::default(),
            open: Box::new(move || {
                let mut m = MockDevice::new(8 * 1024 * 1024);
                m.flash = f2.clone();
                Ok(("MOCK".to_string(), Device::new(Box::new(m) as Box<dyn Transport>)?))
            }),
        };
        (w, flash, root)
    }

    /// First RGB565 pixel of album `n` in a flash image.
    fn first_pixel(flash: &[u8], album: usize) -> u16 {
        let h = album * FRAME_HEADER_SIZE;
        let off = u32::from_le_bytes(flash[h + 12..h + 16].try_into().unwrap()) as usize;
        u16::from_be_bytes([flash[off], flash[off + 1]])
    }

    fn album_count(flash: &[u8]) -> usize {
        (0..MAX_FRAME_HEADERS)
            .take_while(|i| {
                let o = i * FRAME_HEADER_SIZE;
                u32::from_le_bytes(flash[o..o + 4].try_into().unwrap()) == FRAME_MAGIC
            })
            .count()
    }

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    #[test]
    fn sequential_rotation_advances_and_skips_broken_files() {
        let (w, flash, _root) = rig("seq", Order::Sequential, 1, &[RED, GREEN, BLUE]);
        let mut rng = Rng::new();
        // list is [0.png, 1.png, 2.png, broken.png]; expect red, green, blue, red...
        let expect = [0xF800u16, 0x07E0, 0x001F, 0xF800];
        for (n, want) in expect.iter().enumerate() {
            w.rotate(&mut rng).unwrap();
            let f = flash.lock().unwrap();
            assert_eq!(album_count(&f), 1, "step {n}");
            assert_eq!(first_pixel(&f, 0), *want, "step {n}");
        }
        let st = w.state.lock().unwrap();
        assert!(st.last_change > 0);
        assert_eq!(st.last_files, vec!["0.png".to_string()]);
        // state persisted for the next app start
        assert_eq!(State::load(&w.paths).cursor, st.cursor);
    }

    #[test]
    fn multi_image_change_uploads_several_albums() {
        let (w, flash, _root) = rig("multi", Order::Sequential, 2, &[RED, GREEN, BLUE]);
        let mut rng = Rng::new();
        w.rotate(&mut rng).unwrap();
        let f = flash.lock().unwrap();
        assert_eq!(album_count(&f), 2);
        assert_eq!(first_pixel(&f, 0), 0xF800);
        assert_eq!(first_pixel(&f, 1), 0x07E0);
    }

    #[test]
    fn random_rotation_never_repeats_previous_picture() {
        let (w, flash, _root) = rig("rand", Order::Random, 1, &[RED, GREEN, BLUE]);
        let mut rng = Rng::new();
        let mut prev = None;
        for _ in 0..12 {
            w.rotate(&mut rng).unwrap();
            let px = first_pixel(&flash.lock().unwrap(), 0);
            assert_ne!(Some(px), prev);
            prev = Some(px);
        }
    }

    #[test]
    fn empty_or_missing_folder_is_an_error_and_writes_nothing() {
        let (w, flash, root) = rig("empty", Order::Sequential, 1, &[]);
        let mut rng = Rng::new();
        // only broken.png present -> nothing usable
        assert!(w.rotate(&mut rng).is_err());
        assert!(flash.lock().unwrap().is_empty());
        w.settings.lock().unwrap().folder = root.join("nope").display().to_string();
        assert!(w.rotate(&mut rng).is_err());
        assert_eq!(w.state.lock().unwrap().last_change, 0);
    }

    #[test]
    fn manual_upload_rejects_invalid_file_before_touching_device() {
        let (w, flash, root) = rig("manual", Order::Sequential, 1, &[RED]);
        let good = root.join("pics/0.png");
        let bad = root.join("pics/broken.png");
        assert!(w.manual_upload(&[good.clone(), bad], true).is_err());
        assert!(flash.lock().unwrap().is_empty());
        w.manual_upload(&[good], true).unwrap();
        assert_eq!(album_count(&flash.lock().unwrap()), 1);
    }
}
