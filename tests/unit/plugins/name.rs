//! Tests for `src/plugins/name.rs`, attached to it by `#[path]`.

use super::*;
use std::path::{Component, Path};

/// Names from the published stores and the core plugins, one per shape seen in the wild (bare,
/// mixed case, inner space, trailing `+`, scoped, scoped with a dot): a rule that refused one
/// would leave a working plugin uninstallable.
const REAL: &[&str] = &[
    "Avatar",
    "DiscordRPC",
    "RealMAX",
    "Song Downloader",
    "Tidal Tags",
    "DiscordRPC+",
    "PlayerAPI",
    "@squadgazzz/spotify-sync",
    "@akasiek/LuckyLP",
    "@visiuun/3d-cover",
    "@meowarex/audio-visualizer",
    "@seth/CatJam",
    "@luna/lib",
    "@luna/lib.native",
    "my_plugin",
    "unknown",
];

/// Each of these aliases the native root or a neighbour's folder, leaves the folder, names a
/// Windows device, or carries a character outside the allowed set.
const HOSTILE: &[&str] = &[
    "",
    ".",
    "..",
    "./a",
    "a/.",
    "a//b",
    "a/",
    "/a",
    "a/b",
    "a/b/c",
    "x/..",
    "../x",
    "@",
    "@/",
    "@s",
    "@s/",
    "@/x",
    "@../x",
    "@./x",
    "@s/../x",
    "@s/pkg/extra",
    "@@s/x",
    "@s/@x",
    "@s/ b",
    "@ s/b",
    "a\\b",
    "x\\",
    "a:b",
    "C:foo",
    "CON",
    "nul",
    "aux.txt",
    "com1",
    "LPT9",
    "con .txt",
    "@s/CON",
    "@con/x",
    "foo.",
    "foo ",
    "a. ",
    " a",
    ".x",
    "-x",
    "_x",
    "+x",
    "a..b",
    "%",
    "a%20b",
    "a*b",
    "a?b",
    "a\"b",
    "a|b",
    "a<b",
    "a$",
    "a\tb",
    "a\nb",
    "a\0b",
    "caf\u{e9}",
    "a\u{200b}b",
    "a\u{ff0e}b",
];

#[test]
fn every_real_name_is_accepted() {
    let refused: Vec<_> = REAL
        .iter()
        .filter_map(|n| PluginName::parse(n).err())
        .collect();
    assert!(refused.is_empty(), "real names refused: {refused:?}");
}

#[test]
fn every_hostile_name_is_refused() {
    let accepted: Vec<_> = HOSTILE
        .iter()
        .filter(|n| PluginName::parse(n).is_ok())
        .collect();
    assert!(accepted.is_empty(), "hostile names accepted: {accepted:?}");
}

/// What a name becomes on disk: one folder, or two for a scoped name, every component a plain
/// one, spelled exactly as the name. Nothing that could be the base itself or reach out of it.
#[test]
fn a_data_dir_is_strictly_under_its_base() {
    let base = Path::new(if cfg!(windows) {
        "C:\\data\\native"
    } else {
        "/data/native"
    });
    for raw in REAL {
        let dir = PluginName::parse(raw).unwrap().data_dir_under(base);
        let rel = dir.strip_prefix(base).unwrap();
        assert!(
            rel.components().all(|c| matches!(c, Component::Normal(_))),
            "{raw}"
        );
        assert!(
            rel.components()
                .map(|c| c.as_os_str())
                .eq(raw.split('/').map(std::ffi::OsStr::new)),
            "{raw}"
        );
    }
}
