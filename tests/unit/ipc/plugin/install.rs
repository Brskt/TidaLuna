//! Tests for `src/ipc/plugin/install.rs`, attached to it by `#[path]`.

use super::commit_install;
use crate::plugins::name::PluginName;

/// Replying Ok to an install whose trust reset failed kept the old decisions, which answer again
/// for identical bytes. The install fails instead, and the previous one stays as it was.
#[test]
fn a_failed_trust_reset_fails_the_install_and_keeps_the_previous_one() {
    let mut pc = rusqlite::Connection::open_in_memory().unwrap();
    crate::plugins::store::init_schema(&mut pc).unwrap();
    let name = PluginName::parse("FailedReset").unwrap();
    crate::plugins::store::install(&pc, "https://x/p", &name, "{}", "old", "h1").unwrap();
    // No `native_trust` table: the reset's DELETE fails.
    let mut sc = rusqlite::Connection::open_in_memory().unwrap();

    let outcome = commit_install(&pc, &mut sc, "https://x/p", &name, "{}", "new", "h2");

    assert!(outcome.is_err());
    let code: String = pc
        .query_row(
            "SELECT code FROM plugin_code WHERE url = ?1",
            ["https://x/p"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(code, "old");
}
