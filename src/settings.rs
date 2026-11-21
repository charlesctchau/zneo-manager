//! Portable paths, persisted settings/state and the activity log.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::schedule::Order;

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub portable: bool,
}

impl Paths {
    pub fn settings_file(&self) -> PathBuf { self.root.join("config").join("settings.toml") }
    pub fn state_file(&self) -> PathBuf { self.root.join("data").join("state.toml") }
    pub fn backups_dir(&self) -> PathBuf { self.root.join("data").join("backups") }
    pub fn log_file(&self) -> PathBuf { self.root.join("logs").join("app.log") }
    pub fn show_request_file(&self) -> PathBuf { self.root.join("data").join("show.request") }

    /// Portable mode (next to the exe) when writable, per-user folder otherwise.
    pub fn resolve() -> Paths {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        if writable_dir(&exe_dir) {
            return Paths { root: exe_dir, portable: true };
        }
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        Paths { root: base.join("ZNeoManager"), portable: false }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in ["config", "data", "logs"] {
            std::fs::create_dir_all(self.root.join(d))
                .with_context(|| format!("cannot create {}", self.root.join(d).display()))?;
        }
        std::fs::create_dir_all(self.backups_dir())?;
        Ok(())
    }
}

fn writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".zneo_write_test");
    match std::fs::write(&probe, b"x") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "zh" (Traditional Chinese) or "en".
    pub language: String,
    pub minimize_to_tray: bool,
    pub auto_enabled: bool,
    pub folder: String,
    pub include_subfolders: bool,
    pub interval_minutes: u32,
    pub order: Order,
    pub images_per_change: u32,
    pub crop: bool,
    pub change_on_launch: bool,
    pub last_upload_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: "zh".into(),
            minimize_to_tray: true,
            auto_enabled: false,
            folder: String::new(),
            include_subfolders: false,
            interval_minutes: 60,
            order: Order::Sequential,
            images_per_change: 1,
            crop: true,
            change_on_launch: true,
            last_upload_dir: String::new(),
        }
    }
}

impl Settings {
    pub fn load(paths: &Paths) -> Settings {
        std::fs::read_to_string(paths.settings_file())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_atomic(&paths.settings_file(), &toml::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix time of the last successful automatic/“change now” upload.
    pub last_change: u64,
    pub cursor: usize,
    pub last_files: Vec<String>,
}

impl State {
    pub fn load(paths: &Paths) -> State {
        std::fs::read_to_string(paths.state_file())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_atomic(&paths.state_file(), &toml::to_string_pretty(self)?)
    }
}

/// Write via temp file + rename so a crash cannot leave a half-written file.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ------------------------------------------------------------------ log

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: String,
    pub level: Level,
    pub msg: String,
}

pub struct Logger {
    file: PathBuf,
    ring: Mutex<VecDeque<LogEntry>>,
}

const RING_MAX: usize = 500;
const LOG_ROTATE_BYTES: u64 = 2 * 1024 * 1024;

impl Logger {
    pub fn new(file: PathBuf) -> Logger {
        if std::fs::metadata(&file).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(&file, file.with_extension("log.1"));
        }
        Logger { file, ring: Mutex::new(VecDeque::new()) }
    }

    pub fn log(&self, level: Level, msg: impl Into<String>) {
        let msg = msg.into();
        let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) {
            let tag = match level {
                Level::Info => "INFO",
                Level::Warn => "WARN",
                Level::Error => "ERROR",
            };
            let _ = writeln!(f, "{time} {tag} {msg}");
        }
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() >= RING_MAX {
                ring.pop_front();
            }
            ring.push_back(LogEntry { time, level, msg });
        }
    }

    pub fn info(&self, msg: impl Into<String>) { self.log(Level::Info, msg) }
    pub fn warn(&self, msg: impl Into<String>) { self.log(Level::Warn, msg) }
    pub fn error(&self, msg: impl Into<String>) { self.log(Level::Error, msg) }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.ring.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join("zneo_test_settings");
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { root: dir, portable: true };
        paths.ensure_dirs().unwrap();
        assert_eq!(Settings::load(&paths).interval_minutes, 60);
        let mut s = Settings::default();
        s.interval_minutes = 180;
        s.folder = "C:\\Pictures\\中文 資料夾".into();
        s.save(&paths).unwrap();
        let back = Settings::load(&paths);
        assert_eq!(back.interval_minutes, 180);
        assert_eq!(back.folder, "C:\\Pictures\\中文 資料夾");
        // partial file keeps defaults for missing keys
        std::fs::write(paths.settings_file(), "interval_minutes = 5\n").unwrap();
        let p = Settings::load(&paths);
        assert_eq!((p.interval_minutes, p.crop), (5, true));
    }
}
