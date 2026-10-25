//! Tests for `src/ui/deep_link.rs`, attached to it by `#[path]`.

use super::*;

fn target(raw: &str) -> String {
    target_for(raw)
        .unwrap_or_else(|| panic!("expected {raw} to be accepted"))
        .to_string()
}

/// What the renderer is handed, which is not the same string as the one above,
/// since the escapes this file guards against all live in the path alone.
fn route(raw: &str) -> String {
    target_for(raw)
        .unwrap_or_else(|| panic!("expected {raw} to be accepted"))
        .route()
        .to_owned()
}

/// KDE and the Windows shell hand back whatever case the emitter wrote, and a launch line may pad
/// the scheme; a bare scheme is the home page.
#[test]
fn a_link_is_accepted_in_any_case_and_padding_and_an_empty_path_is_the_home_page() {
    for (raw, expected) in [
        ("TIDAL://album/1", "https://desktop.tidal.com/album/1"),
        ("tidal://", "https://desktop.tidal.com/"),
        ("  tidal://mix/abc\n", "https://desktop.tidal.com/mix/abc"),
    ] {
        assert_eq!(target(raw), expected, "{raw:?}");
    }
}

#[test]
fn a_link_that_is_not_ours_or_not_safe_to_route_is_refused() {
    for raw in [
        // A foreign scheme; a route our list holds behind another scheme only the scheme check
        // refuses.
        "https://desktop.tidal.com/album/1",
        "https://album/1",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "tidalx://album/1",
        "tida://album/1",
        "",
        // A real callback arrives inside our own browser; one handed over by the OS carries
        // someone else's code. The last mutates state from its query with no credential at all.
        "tidal://login/auth?code=stolen&state=x",
        "tidal://login",
        "tidal://oauth/spotify/return?claimId=1&code=stolen",
        "tidal://lastfm/auth/stolen-token",
        "tidal://marketplace/checkout/return?productType=ALBUM&itemId=1",
        // Routes taking a query into code nobody here has read, and unknown routes.
        "tidal://view/anything",
        "tidal://upload/share/save/track/1",
        "tidal://loginauth",
        "tidal://whatever-tidal-adds-next",
        // A multi-byte character across byte 8, where the split after `tidal://` cuts: any
        // local process could otherwise make it panic.
        "tidal:/\u{20AC}",
        "tidal:\u{20AC}x",
        "tidal\u{1F600}",
        // A path opening on an empty segment: a browser resolving `//album/1` against our
        // origin reads `album` as the HOST. A backslash parses to the same doubled slash.
        "tidal:///album/1",
        "tidal:////album/1",
        "tidal://///album/1",
        "tidal:///\\album/1",
        "tidal://\\/album/1",
        "tidal:///login/auth?code=stolen",
        "tidal:////login/auth",
        // Only the bare scheme opens the home page; these deliver a path a browser refuses.
        "tidal:///",
        "tidal://////////",
        // A `..` that leaves a doubled slash behind it.
        "tidal://x/..//album/1",
    ] {
        assert!(target_for(raw).is_none(), "{raw:?} was accepted");
    }
}

/// A hostile argument in front does not hide the link behind it.
#[test]
fn the_scheme_argument_is_picked_out_of_a_command_line_and_nothing_when_absent() {
    let table: [(&[&str], Option<&str>); 4] = [
        (
            &["--enable-logging", "tidal://album/1", "--other"],
            Some("tidal://album/1"),
        ),
        (
            &["tida\u{20AC}xy", "tidal://album/1"],
            Some("tidal://album/1"),
        ),
        (&["--type=renderer", "https://example.com"], None),
        (&[], None),
    ];
    for (args, expected) in table {
        let found = url_from_args(args.iter().map(|arg| OsString::from(*arg)));
        assert_eq!(found.as_deref(), expected, "{args:?}");
    }
}

/// A filename the platform accepts and `String` cannot hold, a legacy encoding
/// on unix or an unpaired surrogate on Windows. Both are what `%u` and the shell
/// really hand over, and reading argv as `String` aborts the launch on either.
fn undecodable_argument() -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![
            b'/', b'B', 0xF6, b'r', b'k', b'.', b'f', b'l', b'a', b'c',
        ])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0x002F, 0x0042, 0xD800, 0x0072, 0x006B])
    }
}

#[cfg(any(unix, windows))]
#[test]
fn an_argument_that_is_not_unicode_is_skipped_not_fatal() {
    let args = vec![undecodable_argument(), OsString::from("tidal://album/1")];
    assert_eq!(url_from_args(args), Some("tidal://album/1".to_owned()));
}

/// The allowlist decodes and case-folds the first segment and that spelling is delivered, since the
/// raw one draws a 403 from TIDAL's host. Nothing past it was validated; ids, share codes and
/// escapes stay as written, and a `..` that cancels a leading empty segment leaves its path.
#[test]
fn the_route_delivered_is_the_matched_first_segment_and_the_rest_as_written() {
    for (raw, expected) in [
        ("tidal:///../album/1", "/album/1"),
        ("tidal://Album/1", "/album/1"),
        ("tidal://%61lbum/1", "/album/1"),
        ("tidal://album/AbC%20dEf", "/album/AbC%20dEf"),
        ("tidal://playlist/A-b_C?q=Z%2Fz", "/playlist/A-b_C?q=Z%2Fz"),
    ] {
        assert_eq!(route(raw), expected, "{raw:?}");
    }
}

/// A command line as `classify_launch` receives it, program name already skipped.
fn line(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Whatever someone passes by hand is theirs. A link beside anything else is refused, and the
/// `--type=` line is the one that matters: keying the refusal on that switch let the same broken
/// quote turn it off.
#[test]
fn a_launch_is_let_through_only_when_the_link_is_all_it_carries() {
    let table: [(&[&str], Launch); 9] = [
        (&["tidal://album/1"], Launch::LinkAlone),
        (&["--", "tidal://album/1"], Launch::LinkAlone),
        (&["--", "TIDAL:album/1"], Launch::LinkAlone),
        (&[], Launch::NoLink),
        (&["--enable-logging"], Launch::NoLink),
        (
            &["tidal://x", "--browser-subprocess-path=evil"],
            Launch::LinkAndMore,
        ),
        (
            &["--", "tidal://x", "--browser-subprocess-path=evil"],
            Launch::LinkAndMore,
        ),
        (
            &[
                "--type=renderer",
                "tidal://x",
                "--browser-subprocess-path=evil",
            ],
            Launch::LinkAndMore,
        ),
        (
            &["TIDAL://x", "--browser-subprocess-path=evil"],
            Launch::LinkAndMore,
        ),
    ];
    for (args, expected) in table {
        assert_eq!(classify_launch(&line(args)), expected, "{args:?}");
    }
}
