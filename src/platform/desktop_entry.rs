//! Linux: install a freedesktop `.desktop` entry plus an icon under
//! `~/.local/share/` for the WM to match a taskbar/dock icon for our window.
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
//! the current binary path or embedded icon.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;

const ICON_DATA: &[u8] = include_bytes!("../../tidaluna.png");
pub(crate) const WM_CLASS: &str = "tidalunar";

/// Browser process only: CEF subprocesses re-enter `main()`.
pub(crate) fn install() {
    // A managed install (Nix, etc.) ships its own entry; a user-level one would
    // shadow it, bypass the launch wrapper, and go stale after an upgrade.
    if crate::util::is_managed_install() {
        remove_stale_user_entry();
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
        remove_stale_user_entry();
        return;
    }

    let Some(home) = crate::state::user_home() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    if let Some(size) = parse_png_size(ICON_DATA) {
        install_icon(home, size);
    }
    install_desktop_entry(home, &exe);
}

/// Remove a user-level `.desktop` entry left by a prior unpackaged run.
fn remove_stale_user_entry() {
    if let Some(home) = crate::state::user_home() {
        let stale = home
            .join(".local/share/applications")
            .join(format!("{WM_CLASS}.desktop"));
        let _ = fs::remove_file(&stale);
    }
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

fn install_desktop_entry(home: &Path, exe: &Path) {
    let Some(exec) = exec_value(exe) else {
        crate::verr!("[desktop_entry] Executable path cannot be written to an Exec line");
        return;
    };
    let dir = home.join(".local/share/applications");
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
