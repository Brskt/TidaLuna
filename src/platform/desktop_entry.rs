//! Linux: install a freedesktop `.desktop` entry plus its icons under the user
//! data directory (`$XDG_DATA_HOME`, else `~/.local/share`) for the WM to match
//! a taskbar/dock icon for our window.
//!
//! GNOME Shell ignores `_NET_WM_ICON` and only resolves the icon through a
//! `.desktop` file matched by `WM_CLASS`; KDE and others use both. We always
//! write the entry: the user gets a real icon on every desktop.
//!
//! The entry also carries the `tidal://` association. Recording us as its
//! default handler is a separate job, `claim_scheme_default`, because a
//! packaged install ships the entry and still claims nothing.
//!
//! Idempotent: each call rewrites only when the on-disk content differs from
//! the current binary path or embedded icons.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub(crate) const WM_CLASS: &str = "tidalunar";

/// The `.deb`'s icon of one size, read from that size's directory.
macro_rules! packaged_icon {
    ($size:literal) => {
        (
            $size,
            include_bytes!(concat!(
                "../../installer/linux/deb/icons/hicolor/",
                stringify!($size),
                "x",
                stringify!($size),
                "/apps/tidalunar.png"
            )),
        )
    };
}

/// The sizes the `.deb` ships, all declared by hicolor's `index.theme`. A theme
/// scans only the size directories its system index lists; an icon filed under
/// any other size, such as the source PNG's 1024, never resolves.
const ICONS: [(u32, &[u8]); 5] = [
    packaged_icon!(16),
    packaged_icon!(32),
    packaged_icon!(64),
    packaged_icon!(128),
    packaged_icon!(256),
];

/// Sizes earlier versions filed our icon under, none of them declared by hicolor.
const LEGACY_ICON_SIZES: [u32; 1] = [1024];

/// Browser process only: CEF subprocesses re-enter `main()`.
pub(crate) fn install() {
    let Some(data) = crate::state::user_data_home() else {
        return;
    };

    // A managed install (Nix, etc.) ships its own entry; a user-level one would
    // shadow it, bypass the launch wrapper, and go stale after an upgrade.
    if crate::util::is_managed_install() {
        remove_user_install(&data);
        return;
    }

    // If we're running from a packaged install (.deb), the system already
    // ships /usr/share/applications/tidalunar.desktop pointing at
    // /usr/bin/tidalunar (the launcher). Writing a user-level entry here
    // would shadow the system one and bypass the launcher script (which
    // does first-launch extraction, the protocol-version gate, and the
    // CHROME_DEVEL_SANDBOX env export).
    if Path::new("/usr/bin/tidalunar").exists()
        && Path::new("/usr/share/applications/tidalunar.desktop").exists()
    {
        remove_user_install(&data);
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    let hicolor = data.join("icons/hicolor");
    if install_icons(&hicolor) {
        refresh_icon_theme(&hicolor);
    }
    install_desktop_entry(&data, &exe);
}

/// What a prior unpackaged run left: the entry, and icons that would shadow
/// the package's own under the same name.
fn remove_user_install(data: &Path) {
    let apps = data.join("applications");
    if fs::remove_file(apps.join(format!("{WM_CLASS}.desktop"))).is_ok() {
        refresh_desktop_database(&apps);
    }
    let hicolor = data.join("icons/hicolor");
    let sizes = ICONS.iter().map(|&(size, _)| size);
    if remove_icons(&hicolor, sizes.chain(LEGACY_ICON_SIZES)) {
        refresh_icon_theme(&hicolor);
    }
}

fn size_dir(size: u32) -> String {
    format!("{size}x{size}")
}

/// True when a file was written or removed, for the theme to be told.
fn install_icons(hicolor: &Path) -> bool {
    let mut changed = false;
    for (size, data) in ICONS {
        changed |= install_icon(hicolor, size, data);
    }
    remove_icons(hicolor, LEGACY_ICON_SIZES) | changed
}

fn install_icon(hicolor: &Path, size: u32, data: &[u8]) -> bool {
    let dir = hicolor.join(size_dir(size)).join("apps");
    if fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let path = dir.join(format!("{WM_CLASS}.png"));
    let needs_write = match fs::read(&path) {
        Ok(existing) => existing != data,
        Err(_) => true,
    };
    needs_write && fs::write(&path, data).is_ok()
}

fn remove_icons(hicolor: &Path, sizes: impl IntoIterator<Item = u32>) -> bool {
    let mut removed = false;
    for size in sizes {
        removed |= remove_icon(hicolor, size);
    }
    removed
}

/// Our icon at one size, by its exact path. A symlinked size or `apps` directory
/// is the user's to keep; `remove_dir` refuses a directory still holding another
/// app's icon.
fn remove_icon(hicolor: &Path, size: u32) -> bool {
    let dir = hicolor.join(size_dir(size));
    let apps = dir.join("apps");
    let real_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
    if !real_dir(&dir) || !real_dir(&apps) {
        return false;
    }
    if fs::remove_file(apps.join(format!("{WM_CLASS}.png"))).is_err() {
        return false;
    }
    let _ = fs::remove_dir(&apps);
    let _ = fs::remove_dir(&dir);
    true
}

/// GTK and GNOME Shell rescan a theme only when its top directory's mtime moves;
/// a file under an existing `NxN/apps` leaves it alone. A user cache is rebuilt
/// first, then stamped with the directory's new time to stay valid.
fn refresh_icon_theme(hicolor: &Path) {
    let cache = hicolor.join("icon-theme.cache");
    let cached = cache.is_file();
    if cached {
        let _ = std::process::Command::new("gtk-update-icon-cache")
            .args(["-f", "-t", "-q"])
            .arg(hicolor)
            .output();
    }
    // Whole seconds, compared for equality: a stamp inside the directory's
    // current second would read as no change.
    let Ok(old) = fs::metadata(hicolor).and_then(|meta| meta.modified()) else {
        return;
    };
    let next = SystemTime::now().max(old + Duration::from_secs(1));
    let _ = fs::File::open(hicolor).and_then(|dir| dir.set_modified(next));
    if cached {
        let _ = fs::File::open(&cache).and_then(|file| file.set_modified(next));
    }
}

/// Characters that force an `Exec` argument into double quotes: the spec's
/// reserved set, plus the braces and brackets KDE's splitter reads as shell
/// syntax and would hand to `/bin/sh -c`.
const EXEC_RESERVED: &[char] = &[
    ' ', '\t', '\n', '\r', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
    ')', '`', '{', '}', '[', ']',
];

/// One `Exec` argument, quoted only when it has to be: a path that needs
/// nothing stays byte for byte what earlier versions wrote. `%` is doubled
/// either way, because the launcher reads it as a field code.
fn quote_exec_arg(arg: &str) -> String {
    let quoted = arg.contains(EXEC_RESERVED);
    let mut out = String::with_capacity(arg.len() + 2);
    if quoted {
        out.push('"');
    }
    for c in arg.chars() {
        match c {
            '%' => out.push_str("%%"),
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    if quoted {
        out.push('"');
    }
    out
}

/// The file-level escapes of the spec's `string` type, applied to the whole
/// value after the `Exec` quoting; a quoting backslash is itself doubled.
fn escape_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// The `Exec=` value that opens `exe` on one URL, or `None` when the path
/// cannot be written faithfully. A desktop entry is UTF-8, and a lossy copy of
/// a path that is not would name a file that does not exist; the other control
/// characters have no escape in the `string` type.
///
/// GLib and KDE both look the program up before they expand field codes; a
/// `%%` in the program itself finds nothing. Behind `env` it is an argument,
/// expanded before it is run, unless it holds `=`, which `env` would read as an
/// assignment: that path has no form.
fn exec_value(exe: &Path) -> Option<String> {
    let path = exe.to_str()?;
    if path
        .chars()
        .any(|c| c.is_ascii_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return None;
    }
    let launcher = match (path.contains('%'), path.contains('=')) {
        (true, true) => return None,
        (true, false) => "env ",
        (false, _) => "",
    };
    Some(escape_string(&format!(
        "{launcher}{} %u",
        quote_exec_arg(path)
    )))
}

fn install_desktop_entry(data: &Path, exe: &Path) {
    let Some(exec) = exec_value(exe) else {
        crate::verr!("[desktop_entry] Executable path cannot be written to an Exec line");
        return;
    };
    let dir = data.join("applications");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{WM_CLASS}.desktop"));
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=TidaLunar\n\
         GenericName=Music Player\n\
         Comment=A native TIDAL client\n\
         Exec={exec}\n\
         Icon={WM_CLASS}\n\
         StartupWMClass={WM_CLASS}\n\
         Categories=AudioVideo;Audio;Music;\n\
         MimeType=x-scheme-handler/{scheme};\n\
         Terminal=false\n",
        scheme = crate::ui::deep_link::SCHEME,
    );
    let needs_write = match fs::read_to_string(&path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        let _ = fs::write(&path, contents);
        refresh_desktop_database(&dir);
    }
}

/// A `MimeType=` line is only half the registration. KDE and GNOME both read
/// scheme handlers out of `mimeinfo.cache`, which this rebuilds. Missing on a
/// minimal system, and its absence costs the association until the next session,
/// not the entry itself.
fn refresh_desktop_database(dir: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg("-q")
        .arg(dir)
        .spawn();
}

/// Called on every launch, because a `MimeType=` line only volunteers us. With
/// a second TIDAL client installed and no default recorded, the winner is a byte
/// sort of desktop file ids, and `tidalunar.desktop` sorts after every published
/// one. Claiming each launch hands the links to whichever client the user opened
/// last, which is also how they hand them back (they open the other one).
///
/// Off the boot path, because `xdg-mime` shells out to the desktop's own config
/// tools and its cost cannot be measured from here.
pub(crate) fn claim_scheme_default() {
    std::thread::spawn(|| {
        let target = ClaimTarget::ours();
        let claim = run_xdg_mime(&target);
        report(&claim, &target);
    });
}

/// What we ask the desktop to record, our own entry for our own scheme. Both
/// halves are built from the constants the entry itself is written from; a
/// rename cannot leave the claim naming a file nothing installs.
struct ClaimTarget {
    /// Desktop file id. Every packaging path lands on this same name: the
    /// user-level write above, the `.deb` copy, and the Nix desktop item.
    entry: String,
    mime: String,
}

impl ClaimTarget {
    fn ours() -> Self {
        Self {
            entry: format!("{WM_CLASS}.desktop"),
            mime: format!("x-scheme-handler/{}", crate::ui::deep_link::SCHEME),
        }
    }
}

/// How the claim landed. Four outcomes and not a bool, because they are four
/// different machines: one that recorded it, one that can never record it, one
/// whose tool ran and said no, and one where we never got to ask.
#[derive(Debug, PartialEq, Eq)]
enum Claim {
    Recorded,
    ToolMissing,
    Refused(Option<i32>),
    NotRun(std::io::ErrorKind),
}

/// `xdg-mime default` writes the `[Default Applications]` entry itself. Going
/// through it beats editing `mimeapps.list` from here. Glib's own merge takes no
/// lock between its read and its write, so a second writer is a race we opened.
/// `output` keeps the tool's stdio out of our console.
fn run_xdg_mime(target: &ClaimTarget) -> Claim {
    let outcome = std::process::Command::new("xdg-mime")
        .args(["default", &target.entry, &target.mime])
        .output()
        .map(|done| done.status);
    classify(outcome)
}

/// Split from the run so the mapping is exercised without a subprocess.
fn classify(outcome: std::io::Result<std::process::ExitStatus>) -> Claim {
    match outcome {
        Ok(status) if status.success() => Claim::Recorded,
        Ok(status) => Claim::Refused(status.code()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Claim::ToolMissing,
        Err(e) => Claim::NotRun(e.kind()),
    }
}

fn report(claim: &Claim, target: &ClaimTarget) {
    let ClaimTarget { entry, mime } = target;
    match claim {
        Claim::Recorded => crate::vprintln!("[desktop_entry] {mime} resolves to {entry}"),
        // Expected on a minimal system, and it costs the claim rather than the
        // entry; we stay a candidate, and lose any tie to a lower-sorting id.
        Claim::ToolMissing => crate::vprintln!("[desktop_entry] No xdg-mime, {mime} unclaimed"),
        Claim::Refused(code) => crate::verr!("[desktop_entry] Refused {mime}: exit {code:?}"),
        Claim::NotRun(kind) => crate::verr!("[desktop_entry] No xdg-mime run for {mime}: {kind}"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/platform/desktop_entry.rs"]
mod tests;
