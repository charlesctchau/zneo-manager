//! Windows-specific integration (autostart, tray, dialogs, fonts) with inert
//! fallbacks on other platforms so the rest of the app builds and tests anywhere.

use eframe::egui;
use std::path::PathBuf;

pub const AUTOSTART_VALUE: &str = "ZNeoManager";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Show,
    ChangeNow,
    Quit,
}

/// Procedurally drawn 32x32 icon (a small LCD): no image asset needed.
pub fn icon_rgba() -> (Vec<u8>, u32, u32) {
    let n = 32u32;
    let mut px = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let i = ((y * n + x) * 4) as usize;
            let in_body = (2..30).contains(&x) && (6..26).contains(&y);
            let in_screen = (4..28).contains(&x) && (8..24).contains(&y);
            let c = if in_screen {
                // simple diagonal gradient
                let t = ((x + y) as f32 / 60.0).min(1.0);
                [(40.0 + 60.0 * t) as u8, (160.0 - 40.0 * t) as u8, 230, 255]
            } else if in_body {
                [30, 34, 44, 255]
            } else {
                [0, 0, 0, 0]
            };
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    (px, n, n)
}

// ------------------------------------------------------------------ windows

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::enums::*;
    use winreg::RegKey;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn autostart_value() -> Option<String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_READ)
            .ok()?
            .get_value::<String, _>(AUTOSTART_VALUE)
            .ok()
    }

    pub fn autostart_set(enable: bool) -> Result<(), String> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
            .map_err(|e| e.to_string())?;
        if enable {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = format!("\"{}\" --minimized", exe.display());
            key.set_value(AUTOSTART_VALUE, &cmd).map_err(|e| e.to_string())
        } else {
            match key.delete_value(AUTOSTART_VALUE) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }

    pub fn pick_folder(title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).pick_folder()
    }

    pub fn pick_files(title: &str) -> Vec<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "webp", "gif"])
            .pick_files()
            .unwrap_or_default()
    }

    pub fn open_file(title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).add_filter("Z-NEO backup", &["bak"]).pick_file()
    }

    pub fn open_in_explorer(path: &std::path::Path) {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }

    pub struct Tray {
        _icon: tray_icon::TrayIcon,
    }

    impl Tray {
        pub fn new(
            ctx: egui::Context,
            zh: bool,
            on_cmd: impl Fn(TrayCmd) + Send + Sync + 'static,
        ) -> Option<Tray> {
            use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
            use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

            let (rgba, w, h) = icon_rgba();
            let icon = Icon::from_rgba(rgba, w, h).ok()?;
            let show = MenuItem::new(if zh { "顯示視窗" } else { "Show window" }, true, None);
            let now = MenuItem::new(if zh { "立即換圖" } else { "Change picture now" }, true, None);
            let quit = MenuItem::new(if zh { "結束" } else { "Quit" }, true, None);
            let menu = Menu::new();
            menu.append(&show).ok()?;
            menu.append(&now).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&quit).ok()?;
            let (show_id, now_id, quit_id) = (show.id().clone(), now.id().clone(), quit.id().clone());

            let on_cmd = std::sync::Arc::new(on_cmd);
            let c1 = on_cmd.clone();
            let ctx1 = ctx.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let cmd = if e.id == show_id {
                    TrayCmd::Show
                } else if e.id == now_id {
                    TrayCmd::ChangeNow
                } else if e.id == quit_id {
                    TrayCmd::Quit
                } else {
                    return;
                };
                c1(cmd);
                ctx1.request_repaint();
            }));
            let c2 = on_cmd;
            let ctx2 = ctx;
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if matches!(e, TrayIconEvent::DoubleClick { .. }) {
                    c2(TrayCmd::Show);
                    ctx2.request_repaint();
                }
            }));

            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip("Z-NEO Manager")
                .with_icon(icon)
                .build()
                .ok()?;
            Some(Tray { _icon: tray })
        }
    }

    /// First available CJK-capable Windows font.
    pub fn cjk_font_bytes() -> Option<Vec<u8>> {
        let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        for name in ["msjh.ttc", "msjh.ttf", "msyh.ttc", "mingliu.ttc", "simsun.ttc"] {
            if let Ok(b) = std::fs::read(dir.join("Fonts").join(name)) {
                return Some(b);
            }
        }
        None
    }
}

// -------------------------------------------------------------- non-windows

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn autostart_value() -> Option<String> { None }
    pub fn autostart_set(_enable: bool) -> Result<(), String> {
        Err("autostart is only supported on Windows".into())
    }
    pub fn pick_folder(_t: &str) -> Option<PathBuf> { None }
    pub fn pick_files(_t: &str) -> Vec<PathBuf> { Vec::new() }
    pub fn open_file(_t: &str) -> Option<PathBuf> { None }
    pub fn open_in_explorer(_p: &std::path::Path) {}
    pub struct Tray;
    impl Tray {
        pub fn new(_c: egui::Context, _zh: bool, _f: impl Fn(TrayCmd) + Send + Sync + 'static) -> Option<Tray> {
            None
        }
    }
    pub fn cjk_font_bytes() -> Option<Vec<u8>> { None }
}

pub use imp::*;

/// Is the registered autostart command pointing at the running executable?
pub fn autostart_points_here() -> bool {
    let Some(v) = autostart_value() else { return false };
    let Ok(exe) = std::env::current_exe() else { return false };
    v.contains(&exe.display().to_string())
}
