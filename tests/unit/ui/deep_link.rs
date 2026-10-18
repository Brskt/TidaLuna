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

#[test]
fn accepts_the_scheme_in_any_case() {
    // KDE and the Windows shell hand back whatever case the emitter wrote.
    assert_eq!(
        target("TIDAL://album/1"),
        "https://desktop.tidal.com/album/1"
    );
}

#[test]
fn an_empty_path_lands_on_the_home_page() {
    assert_eq!(target("tidal://"), "https://desktop.tidal.com/");
}

#[test]
fn the_content_routes_still_open() {
    for (raw, expected) in [
        (
            "tidal://album/175122451",
            "https://desktop.tidal.com/album/175122451",
        ),
        ("tidal://track/9", "https://desktop.tidal.com/track/9"),
        ("tidal://artist/7", "https://desktop.tidal.com/artist/7"),
        (
            "tidal://playlist/abc-def",
            "https://desktop.tidal.com/playlist/abc-def",
        ),
        ("tidal://mix/m1", "https://desktop.tidal.com/mix/m1"),
        ("tidal://video/v1", "https://desktop.tidal.com/video/v1"),
        ("tidal://search?q=x", "https://desktop.tidal.com/search?q=x"),
        (
            "tidal://my-collection/albums",
            "https://desktop.tidal.com/my-collection/albums",
        ),
    ] {
        assert_eq!(target(raw), expected);
    }
}

#[test]
fn a_foreign_scheme_is_not_ours_to_route() {
    assert!(target_for("https://desktop.tidal.com/album/1").is_none());
    // A route our list holds, behind another scheme: only the scheme check refuses it.
    assert!(target_for("https://album/1").is_none());
    assert!(target_for("file:///etc/passwd").is_none());
    assert!(target_for("javascript:alert(1)").is_none());
    assert!(target_for("tidalx://album/1").is_none());
    assert!(target_for("tida://album/1").is_none());
    assert!(target_for("").is_none());
}

#[test]
fn every_route_that_redeems_a_credential_is_refused() {
    // A real callback arrives as a navigation inside our own browser; one handed
    // over by the OS carries someone else's code. `/login` was never the only
    // one. These three redeem a credential, and the fourth mutates state from
    // its query string with no credential at all.
    assert!(target_for("tidal://login/auth?code=stolen&state=x").is_none());
    assert!(target_for("tidal://login").is_none());
    assert!(target_for("tidal://oauth/spotify/return?claimId=1&code=stolen").is_none());
    assert!(target_for("tidal://lastfm/auth/stolen-token").is_none());
    assert!(target_for("tidal://marketplace/checkout/return?productType=ALBUM&itemId=1").is_none());
}

#[test]
fn a_route_nobody_has_read_is_refused_until_someone_has() {
    for raw in [
        // Both take a whole query string into code this project has not read.
        "tidal://view/anything",
        "tidal://upload/share/save/track/1",
        // And an unknown route is refused rather than guessed at.
        "tidal://loginauth",
        "tidal://whatever-tidal-adds-next",
    ] {
        assert!(target_for(raw).is_none(), "{raw} was accepted");
    }
}

#[test]
fn a_character_straddling_the_scheme_length_is_refused_not_fatal() {
    // Each of these puts a multi-byte character across byte 8, where the split
    // after `tidal://` cuts. They arrive from a launch argument and from the two
    // listeners, making a panic here any local process's to trigger.
    for raw in ["tidal:/\u{20AC}", "tidal:\u{20AC}x", "tidal\u{1F600}"] {
        assert!(target_for(raw).is_none(), "{raw} was accepted");
    }
}

#[test]
fn picks_the_scheme_argument_out_of_a_command_line() {
    let args = ["--enable-logging", "tidal://album/1", "--other"].map(OsString::from);
    assert_eq!(url_from_args(args), Some("tidal://album/1".to_owned()));
    // A hostile argument in front does not hide the link behind it.
    let args = ["tida\u{20AC}xy", "tidal://album/1"].map(OsString::from);
    assert_eq!(url_from_args(args), Some("tidal://album/1".to_owned()));
}

#[test]
fn a_command_line_without_our_scheme_yields_nothing() {
    let args = ["--type=renderer", "https://example.com"].map(OsString::from);
    assert_eq!(url_from_args(args), None);
    assert_eq!(url_from_args(Vec::new()), None);
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

#[test]
fn surrounding_whitespace_does_not_defeat_the_scheme_check() {
    assert_eq!(
        target("  tidal://mix/abc\n"),
        "https://desktop.tidal.com/mix/abc"
    );
}

#[test]
fn a_path_opening_on_an_empty_segment_is_refused() {
    for raw in [
        // The allowlist reads the first non-empty segment; `album` answered for
        // all three of these. What travels is the whole path, and a browser
        // resolving `//album/1` against our origin reads `album` as the HOST.
        "tidal:///album/1",
        "tidal:////album/1",
        "tidal://///album/1",
        // A special scheme takes a backslash for a path separator, parsing both of
        // these to the doubled slash above. Naming the `//` spelling alone would
        // leave the other one open, which is how the same fix failed elsewhere.
        "tidal:///\\album/1",
        "tidal://\\/album/1",
        // `tidal:///login/auth` grafts to a path of `//login/auth`, whose first
        // segment is empty. Reading that as "the home page" would hand the whole
        // allowlist a way round itself.
        "tidal:///login/auth?code=stolen",
        "tidal:////login/auth",
        // Only the bare scheme opens the home page. These deliver a path a browser
        // refuses outright, which reaches the renderer as a thrown exception.
        "tidal:///",
        "tidal://////////",
    ] {
        assert!(target_for(raw).is_none(), "{raw} was accepted");
    }
}

#[test]
fn a_traversal_that_lands_on_a_doubled_slash_is_refused() {
    // Whether `..` neutralises the extra slash or leaves it depends on where it
    // sits; the shape of the result is what decides, not the presence of one.
    assert!(target_for("tidal://x/..//album/1").is_none());
    assert_eq!(route("tidal:///../album/1"), "/album/1");
}

#[test]
fn the_route_delivered_is_the_spelling_that_was_matched() {
    // Both are accepted because the allowlist decodes and case-folds first.
    // Delivering the raw spelling instead points the launch at a URL TIDAL's
    // own host answers with a 403, and the app never boots.
    assert_eq!(route("tidal://Album/1"), "/album/1");
    assert_eq!(route("tidal://%61lbum/1"), "/album/1");
}

#[test]
fn only_the_segment_that_was_matched_is_rewritten() {
    // Nothing past the first segment was validated, leaving nothing there
    // normalised. Ids and share codes are case-sensitive, and re-encoding an
    // escape would change what it means.
    assert_eq!(route("tidal://album/AbC%20dEf"), "/album/AbC%20dEf");
    assert_eq!(
        route("tidal://playlist/A-b_C?q=Z%2Fz"),
        "/playlist/A-b_C?q=Z%2Fz"
    );
}

/// A command line as `classify_launch` receives it, program name already skipped.
fn line(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn a_launch_carrying_only_the_link_is_let_through() {
    for args in [
        &["tidal://album/1"][..],
        &["--", "tidal://album/1"],
        &["--", "TIDAL:album/1"],
    ] {
        assert_eq!(classify_launch(&line(args)), Launch::LinkAlone, "{args:?}");
    }
}

#[test]
fn a_launch_that_names_no_link_is_left_alone() {
    // Whatever someone passes by hand is theirs.
    assert_eq!(classify_launch(&line(&[])), Launch::NoLink);
    assert_eq!(
        classify_launch(&line(&["--enable-logging"])),
        Launch::NoLink
    );
}

#[test]
fn a_link_beside_anything_else_is_refused() {
    // The `--type=` line is the one that matters: keying this refusal on that
    // switch let the same broken quote turn it off.
    for args in [
        &["tidal://x", "--browser-subprocess-path=evil"][..],
        &["--", "tidal://x", "--browser-subprocess-path=evil"],
        &[
            "--type=renderer",
            "tidal://x",
            "--browser-subprocess-path=evil",
        ],
        &["TIDAL://x", "--browser-subprocess-path=evil"],
    ] {
        assert_eq!(
            classify_launch(&line(args)),
            Launch::LinkAndMore,
            "{args:?}"
        );
    }
}
