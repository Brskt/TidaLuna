//! Tests for `src/ui/hosts.rs`, attached to it by `#[path]`.

use super::*;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap_or_else(|e| panic!("{s} does not parse: {e}"))
}

/// The shape is what `url` parsed: an IP literal is no name whatever its text, and an empty label or
/// a final dot makes a host malformed rather than one more subdomain.
#[test]
fn a_host_is_a_name_a_malformed_domain_or_not_a_name() {
    for (raw, expected) in [
        (
            "https://listen.tidal.com/x",
            HostShape::Name(Name("listen.tidal.com")),
        ),
        ("https://tidal.com./x", HostShape::Malformed("tidal.com.")),
        (
            "https://a..tidal.com/x",
            HostShape::Malformed("a..tidal.com"),
        ),
        ("https://127.0.0.1/x", HostShape::NotAName),
        ("blob:https://desktop.tidal.com/uuid", HostShape::NotAName),
    ] {
        assert_eq!(HostShape::of(&url(raw)), expected, "{raw}");
    }
}

/// A name belongs to a TIDAL domain on a label boundary, and no apex reaches another.
#[test]
fn a_name_is_in_a_tidal_domain_only_on_a_label_boundary() {
    for (raw, expected) in [
        ("https://tidal.com/", Some(TidalDomain::Tidal)),
        ("https://api.tidal.com/", Some(TidalDomain::Tidal)),
        ("https://api.tidalhifi.com/", Some(TidalDomain::TidalHifi)),
        (
            "https://event-collector.eu.tidalhi.fi/",
            Some(TidalDomain::TidalHi),
        ),
        ("https://xtidalhifi.com/", None),
    ] {
        let parsed = url(raw);
        let HostShape::Name(name) = HostShape::of(&parsed) else {
            panic!("{raw} is not a name");
        };
        assert_eq!(name.domain(), expected, "{raw}");
    }
}

/// Every sign-in host Rust must never reach, in any position of any TIDAL domain, and every
/// malformed TIDAL host; the rest stays open, IP literals included.
#[test]
fn the_host_verdict_refuses_sign_in_and_malformed_tidal_hosts() {
    use HostVerdict::{MalformedTidal, Open, SignIn};
    for (raw, expected) in [
        ("https://auth.tidal.com/v1/oauth2/token", SignIn),
        ("https://login.tidal.com/authorize", SignIn),
        ("https://x.auth.tidal.com/", SignIn),
        ("https://auth.stage.tidal.com/", SignIn),
        ("https://login.tidalhifi.com/", SignIn),
        ("https://auth.tidal.com./", MalformedTidal),
        ("https://a..tidal.com/", MalformedTidal),
        ("https://tidal.com./", MalformedTidal),
        ("https://api.tidalhifi.com../", MalformedTidal),
        ("https://api.tidal.com/v1/tracks", Open),
        ("https://desktop.tidal.com/", Open),
        ("https://resources.tidal.com/images/a.jpg", Open),
        ("https://authorize.tidal.com/", Open),
        ("https://auth.evil.com/", Open),
        ("https://auth.tidal.com.evil.com/", Open),
        ("https://evil.com./", Open),
        ("http://127.0.0.1:8080/track.flac", Open),
    ] {
        assert_eq!(HostVerdict::of(&url(raw)), expected, "{raw}");
    }
}
