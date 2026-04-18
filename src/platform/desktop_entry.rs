//! Linux: install a freedesktop `.desktop` entry plus an icon under
//! `~/.local/share/` so the WM can match a taskbar/dock icon for our window.
//!
//! GNOME Shell ignores `_NET_WM_ICON` and only resolves the icon through a
//! `.desktop` file matched by `WM_CLASS`; KDE and others use both. We always
//! write the entry so the user gets a real icon on every desktop.
//!
//! Idempotent: each call rewrites only when the on-disk content differs from
//! the current binary path or embedded icon.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};

const ICON_DATA: &[u8] = include_bytes!("../../tidaluna.png");
pub(crate) const WM_CLASS: &str = "tidalunar";

pub(crate) fn install() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    if let Some(size) = parse_png_size(ICON_DATA) {
        install_icon(&home, size);
    }
    install_desktop_entry(&home, &exe);
}

fn parse_png_size(data: &[u8]) -> Option<u32> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some(width.max(height))
}

fn install_icon(home: &Path, size: u32) {
    let dir = home
        .join(".local/share/icons/hicolor")
        .join(format!("{size}x{size}"))
        .join("apps");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{WM_CLASS}.png"));
    let needs_write = match fs::read(&path) {
        Ok(existing) => existing != ICON_DATA,
        Err(_) => true,
    };
    if needs_write {
        let _ = fs::write(&path, ICON_DATA);
    }
}

fn install_desktop_entry(home: &Path, exe: &Path) {
    let dir = home.join(".local/share/applications");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{WM_CLASS}.desktop"));
    let exe_str = exe.display();
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=TidaLunar\n\
         GenericName=Music Player\n\
         Comment=A native TIDAL client\n\
         Exec={exe_str}\n\
         Icon={WM_CLASS}\n\
         StartupWMClass={WM_CLASS}\n\
         Categories=AudioVideo;Audio;Music;\n\
         Terminal=false\n",
    );
    let needs_write = match fs::read_to_string(&path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        let _ = fs::write(&path, contents);
    }
}
