//! Tests for `src/platform/desktop_entry.rs`, attached by `#[path]`.

use std::ffi::OsStr;
use std::io::{Error, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

use super::{
    Claim, ClaimTarget, EXEC_RESERVED, WM_CLASS, classify, escape_string, exec_value,
    install_desktop_entry, quote_exec_arg,
};
use crate::ui::deep_link::SCHEME;

/// The `.deb` entry, pulled in so a rename of either constant has to break here.
/// Nothing derives this file from them; it spells both out by hand.
const DEB_ENTRY: &str = include_str!("../../../installer/linux/deb/tidalunar.desktop.in");
/// The Nix desktop item, hand-written for the same reason.
const NIX_PACKAGE: &str = include_str!("../../../nix/package.nix");

/// `ExitStatus::from_raw` takes a wait status, not an exit code (the code lives
/// in the second byte).
fn exited(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

#[test]
fn the_claim_names_our_own_entry_and_our_own_scheme() {
    // Both halves are the drift guard. Two packaging files spell the scheme out
    // by hand, and a rename here that they miss has to fail somewhere.
    let target = ClaimTarget::ours();
    assert_eq!(target.entry, "tidalunar.desktop");
    assert_eq!(target.mime, "x-scheme-handler/tidal");
}

#[test]
fn a_clean_exit_records_the_claim() {
    assert_eq!(classify(Ok(exited(0))), Claim::Recorded);
}

#[test]
fn a_missing_xdg_mime_is_its_own_outcome() {
    // Not a failure to report loudly, since the machine simply has no way to
    // record a default and the entry is still a candidate.
    let absent = Err(Error::from(ErrorKind::NotFound));
    assert_eq!(classify(absent), Claim::ToolMissing);
}

#[test]
fn a_nonzero_exit_is_a_refusal_and_carries_its_code() {
    assert_eq!(classify(Ok(exited(1))), Claim::Refused(Some(1)));
}

#[test]
fn a_spawn_failure_is_not_a_refusal() {
    // The distinction the outcome exists for. Nothing was asked, so nothing
    // said no.
    let denied = Err(Error::from(ErrorKind::PermissionDenied));
    assert_eq!(classify(denied), Claim::NotRun(ErrorKind::PermissionDenied));
}

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
fn a_path_with_nothing_to_escape_is_written_as_before() {
    // Byte for byte what the entry held before the quoting existed; existing
    // entries are not rewritten on the next launch.
    let exe = Path::new("/opt/fixture-app/1.0/tidalunar");
    assert_eq!(
        exec_value(exe).as_deref(),
        Some("/opt/fixture-app/1.0/tidalunar %u")
    );
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

#[test]
fn a_path_env_could_not_carry_has_no_form() {
    let exe = Path::new("/opt/a=b%/tidalunar");
    assert_eq!(exec_value(exe), None);
}

#[test]
fn a_control_character_with_no_escape_is_not_written() {
    for c in ['\u{1}', '\u{1b}', '\u{7f}'] {
        let exe = format!("/opt/fixture/{c}/tidalunar");
        assert_eq!(exec_value(Path::new(&exe)), None, "{c:?}");
    }
}

#[test]
fn every_reserved_character_forces_the_quotes() {
    for c in EXEC_RESERVED {
        let quoted = quote_exec_arg(&format!("/opt/a{c}b"));
        assert!(quoted.starts_with('"') && quoted.ends_with('"'), "{c:?}");
    }
}

#[test]
fn only_the_string_escapes_are_applied_to_the_whole_value() {
    assert_eq!(escape_string("a\\b\nc\td\re"), "a\\\\b\\nc\\td\\re");
    assert_eq!(escape_string("plain %u"), "plain %u");
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
