//! Tests for `src/platform/desktop_entry.rs`, attached by `#[path]`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{WM_CLASS, exec_value, install_desktop_entry};
use crate::ui::deep_link::SCHEME;

/// The `.deb` entry, pulled in so a rename of either constant has to break here.
/// Nothing derives this file from them; it spells both out by hand.
const DEB_ENTRY: &str = include_str!("../../../installer/linux/deb/tidalunar.desktop.in");
/// The Nix desktop item, hand-written for the same reason.
const NIX_PACKAGE: &str = include_str!("../../../nix/package.nix");

#[test]
fn the_deb_entry_declares_the_scheme_and_class_the_code_uses() {
    // The name it lands under is chosen in `xtask` rather than in the file,
    // leaving that third copy of the id unchecked by this.
    let mime = format!("MimeType=x-scheme-handler/{SCHEME};");
    let class = format!("StartupWMClass={WM_CLASS}");
    assert!(DEB_ENTRY.contains(&mime), "the .deb entry has no {mime}");
    assert!(DEB_ENTRY.contains(&class), "the .deb entry has no {class}");
}

#[test]
fn the_nix_desktop_item_names_the_entry_we_claim() {
    // `name` decides what the generated file is called, so it is the id that
    // `ClaimTarget::ours` hands to xdg-mime on a Nix install. A substring is as
    // far as this goes; the test reads Nix source rather than evaluating it.
    let name = format!("name = \"{WM_CLASS}\"");
    let mime = format!("\"x-scheme-handler/{SCHEME}\"");
    assert!(NIX_PACKAGE.contains(&name), "the Nix item has no {name}");
    assert!(NIX_PACKAGE.contains(&mime), "the Nix item has no {mime}");
}

/// What a launcher recovers from an `Exec=` value, in the spec's reverse order:
/// the file-level string escapes undone first, then the argument quoting, then
/// `%%` to `%`. Field codes other than `%%` stay as typed.
fn launch_argv(value: &str) -> Vec<String> {
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        text.push(match chars.next() {
            Some('s') => ' ',
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('\\') => '\\',
            other => panic!("invalid string escape {other:?} in {value:?}"),
        });
    }

    let mut argv = Vec::new();
    let mut arg = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match chars.next() {
                Some(c @ ('"' | '\\' | '`' | '$')) => arg.push(c),
                other => panic!("invalid quoted escape {other:?} in {text:?}"),
            },
            ' ' if !quoted => {
                if started {
                    argv.push(std::mem::take(&mut arg));
                    started = false;
                }
            }
            c => {
                arg.push(c);
                started = true;
            }
        }
    }
    assert!(!quoted, "unterminated quote in {text:?}");
    if started {
        argv.push(arg);
    }
    argv.into_iter().map(|arg| arg.replace("%%", "%")).collect()
}

/// Characters and sequences that can break an `Exec` line, each a whole directory name.
const TRAPS: &[&str] = &[
    " ",
    "\t",
    "\n",
    "\r",
    "\"",
    "'",
    "\\",
    ">",
    "<",
    "~",
    "|",
    "&",
    ";",
    "$",
    "*",
    "?",
    "#",
    "(",
    ")",
    "`",
    "{",
    "}",
    "[",
    "]",
    "%",
    "%f",
    "%%",
    "%u",
    "=",
    "\\n",
    "\\s",
    "\\\\",
    "\\\"",
    "$HOME",
    "$(id)",
    "a b\"c`d$e\\f",
    "caf\u{e9}",
    "+@,:!^-_.",
];

fn fixture_exe(dir_name: &str) -> String {
    format!("/opt/fixture/{dir_name}/tidalunar")
}

#[test]
fn the_four_quoted_characters_get_a_backslash_and_the_file_doubles_it() {
    // The spec's own example: a literal dollar sign in a quoted argument is
    // `\\$` in the file, and a literal backslash is four.
    let cases = [
        ("\"", "\"/opt/a\\\\\"b/t\" %u"),
        ("`", "\"/opt/a\\\\`b/t\" %u"),
        ("$", "\"/opt/a\\\\$b/t\" %u"),
        ("\\", "\"/opt/a\\\\\\\\b/t\" %u"),
    ];
    for (c, expected) in cases {
        let exe = format!("/opt/a{c}b/t");
        assert_eq!(
            exec_value(Path::new(&exe)).as_deref(),
            Some(expected),
            "{c:?}"
        );
    }
}

/// A path `env` cannot carry, or one holding a control character with no escape, has no form.
#[test]
fn a_path_the_entry_cannot_carry_has_no_exec_value() {
    for exe in [
        "/opt/a=b%/tidalunar",
        "/opt/fixture/\u{1}/tidalunar",
        "/opt/fixture/\u{1b}/tidalunar",
        "/opt/fixture/\u{7f}/tidalunar",
    ] {
        assert_eq!(exec_value(Path::new(exe)), None, "{exe:?}");
    }
}

#[test]
fn a_launcher_recovers_the_exact_path_and_the_url_slot() {
    for trap in TRAPS {
        let exe = fixture_exe(trap);
        let value = exec_value(Path::new(&exe)).unwrap_or_else(|| panic!("no value for {trap:?}"));
        let mut expected = vec![exe.clone(), "%u".to_owned()];
        if trap.contains('%') {
            expected.insert(0, "env".to_owned());
        }
        assert_eq!(launch_argv(&value), expected, "{trap:?} -> {value:?}");
    }
}

#[test]
fn the_written_entry_carries_the_quoted_line_and_no_other_exec() {
    let home = tempfile::tempdir().expect("a temp dir");
    let exe = Path::new("/opt/fixture apps/tidalunar");
    install_desktop_entry(home.path(), exe);

    let written = std::fs::read_to_string(
        home.path()
            .join(".local/share/applications/tidalunar.desktop"),
    )
    .expect("the entry");
    let execs: Vec<&str> = written.lines().filter(|l| l.starts_with("Exec=")).collect();
    assert_eq!(execs, ["Exec=\"/opt/fixture apps/tidalunar\" %u"]);
}

#[test]
fn a_path_that_is_not_text_leaves_no_entry() {
    let home = tempfile::tempdir().expect("a temp dir");
    let exe = Path::new(OsStr::from_bytes(b"/opt/fixture/\xff/tidalunar"));
    install_desktop_entry(home.path(), exe);

    assert!(!home.path().join(".local/share/applications").exists());
}
