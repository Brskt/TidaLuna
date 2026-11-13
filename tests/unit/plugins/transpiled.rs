//! Tests for `src/plugins/transpiled.rs`, attached to it by `#[path]`.

use super::*;

const NAME: &str = "https://x/p.mts";

fn cache() -> (TranspileCache, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (TranspileCache::new(dir.path().join("plugin_js")), dir)
}

fn file_of(cache: &TranspileCache, name: &str, code: &str) -> PathBuf {
    cache.dir.join(Key::of(name, code).file_name())
}

/// A kept transpile runs in place of the helper's output. Served for another source, or under a
/// name that makes the source a declaration file, it would run code the plugin never shipped.
#[test]
fn a_kept_transpile_serves_only_its_own_source() {
    let (cache, _dir) = cache();
    cache.put(NAME, "source v1", "js of v1").unwrap();

    assert_eq!(cache.get(NAME, "source v1").as_deref(), Some("js of v1"));
    assert_eq!(
        cache.get(NAME, "source v2"),
        None,
        "served for another source"
    );
    assert_eq!(
        cache.get("https://x/p.d.mts", "source v1"),
        None,
        "served to a declaration file"
    );
}

/// A file a power cut tore is a miss, never code.
#[test]
fn a_file_not_read_back_whole_is_a_miss() {
    let (cache, _dir) = cache();
    cache.put(NAME, "source v1", "js of v1").unwrap();
    assert_eq!(cache.get(NAME, "source v1").as_deref(), Some("js of v1"));

    let path = file_of(&cache, NAME, "source v1");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.pop();
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(cache.get(NAME, "source v1"), None);
}

/// A plugin with a folder grant can create a directory at a file's name. The put fails at once:
/// on Windows the rename would retry it for its whole budget, at every load of that plugin.
#[test]
fn a_put_over_a_planted_directory_fails_at_once() {
    let (cache, _dir) = cache();
    std::fs::create_dir_all(file_of(&cache, NAME, "source v1")).unwrap();

    let refused = cache.put(NAME, "source v1", "js of v1").unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(cache.get(NAME, "source v1"), None);
}

/// Plugins sharing a source share its file, whatever their urls. That holds only while nothing of
/// the name but a declaration suffix reaches oxc's output.
#[test]
fn only_a_declaration_name_changes_the_transpile() {
    let source = "class C { m(): void; }";
    let out = |name: &str| crate::plugins::transpile::transpile_ts(source, name).ok();
    let plain = out("https://a/p.mts");
    assert!(plain.as_deref().is_some_and(|js| !js.is_empty()));
    for name in [
        "https://b/q.mts",
        "https://c/p.tsx.mts",
        "https://d/p?x#y.mts",
        "https://e/p.D.mts",
    ] {
        assert_eq!(out(name), plain, "{name}");
    }
    assert_ne!(out("https://f/p.d.mts"), plain);
}
