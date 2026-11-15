//! Tests for `src/plugins/transpile_helper.rs`, attached to it by `#[path]`.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use super::*;

/// The signal that ends the app reaches its whole process group, a helper mid-transpile included.
/// Read as a crash on the source it would disable a plugin that works; a crash of the helper's
/// own still blames the source.
#[test]
fn a_helper_stopped_from_outside_blames_no_plugin() {
    for (sig, blames_the_source, why) in [
        (
            libc::SIGTERM,
            false,
            "a group SIGTERM: kill, systemd, a logout",
        ),
        (
            libc::SIGINT,
            false,
            "Ctrl+C in the terminal that started the app",
        ),
        (
            libc::SIGHUP,
            false,
            "the terminal that started the app closing",
        ),
        (
            libc::SIGABRT,
            true,
            "oxc's stack overflow on a nested source",
        ),
        (
            libc::SIGKILL,
            true,
            "the OOM killer, on a source that pulled the memory up",
        ),
    ] {
        // A raw wait status holding only a signal number is a death by that signal.
        let failure = decode_answer(ExitStatus::from_raw(sig).into(), b"")
            .expect_err("a helper killed by a signal produced nothing");
        assert_eq!(
            matches!(failure, Failure::Source(_)),
            blames_the_source,
            "{why}: {failure}"
        );
    }
}
