//! Tests for `src/video/codec.rs`, attached by `#[path]`.

use super::*;

#[test]
fn every_target_carries_a_full_sha256() {
    assert_eq!(TARGETS.len(), 6, "six shipped platforms, no more");
    for t in TARGETS {
        assert_eq!(t.sha256.len(), 64, "{} digest length", t.file_name);
        assert!(
            t.sha256
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{} digest is lowercase hex",
            t.file_name
        );
    }
}

#[test]
fn each_target_url_matches_its_own_platform_shape() {
    let by_file = |name: &str| TARGETS.iter().find(|t| t.file_name == name).unwrap();

    assert_eq!(
        by_file("openh264-2.6.0-win64.dll").url(),
        "https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
    );
    assert_eq!(
        by_file("libopenh264-2.6.0-mac-arm64.dylib").url(),
        "https://ciscobinary.openh264.org/libopenh264-2.6.0-mac-arm64.dylib.bz2"
    );
    assert_eq!(
        by_file("libopenh264-2.6.0-linux64.8.so").url(),
        "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2"
    );
}

/// Cisco's own release notes link http.
#[test]
fn every_target_url_is_https_and_bzip2() {
    for t in TARGETS {
        let url = t.url();
        assert!(
            url.starts_with("https://ciscobinary.openh264.org/"),
            "{url}"
        );
        assert!(url.ends_with(".bz2"), "{url}");
    }
}

/// The defect that disqualified the crate's own table (see `TARGETS`), pinned rather than assumed.
#[test]
fn no_two_targets_share_a_digest() {
    for (i, a) in TARGETS.iter().enumerate() {
        for b in &TARGETS[i + 1..] {
            assert_ne!(a.sha256, b.sha256, "{} and {}", a.file_name, b.file_name);
        }
    }
}

/// `current_target` takes the first match: a second entry for one platform would never be read.
#[test]
fn no_two_targets_share_a_platform() {
    for (i, a) in TARGETS.iter().enumerate() {
        for b in &TARGETS[i + 1..] {
            assert!(
                (a.os, a.arch) != (b.os, b.arch),
                "{} and {}",
                a.file_name,
                b.file_name
            );
        }
    }
}

/// The build platform must resolve, or this build ships a switch that can never turn on.
#[test]
fn the_current_platform_resolves_to_a_target() {
    let target = current_target().expect("this platform has no OpenH264 build in the table");
    assert!(target.url().contains("2.6.0"));
}

#[test]
fn the_codec_host_is_accepted() {
    assert!(is_codec_host("ciscobinary.openh264.org"));
}

#[test]
fn lookalike_codec_hosts_are_refused() {
    assert!(!is_codec_host("ciscobinary.openh264.org.evil.com"));
    assert!(!is_codec_host("evil-ciscobinary.openh264.org"));
    assert!(!is_codec_host("openh264.org"));
    assert!(!is_codec_host("sub.ciscobinary.openh264.org"));
}

#[test]
fn tidal_hosts_are_not_admitted_by_the_codec_policy() {
    assert!(!is_codec_host("audio.tidal.com"));
    assert!(!is_codec_host("resources.tidal.com"));
    assert!(!is_codec_host("desktop.tidal.com"));
}

/// The host is taken from the parsed url: `https://ciscobinary.openh264.org@evil.com/` reads
/// as `evil.com`. Plaintext is refused outright.
#[test]
fn a_userinfo_prefix_does_not_impersonate_the_codec_host() {
    assert!(!codec_url_allowed(
        "https://ciscobinary.openh264.org@evil.com/x.bz2"
    ));
    assert!(!codec_url_allowed("http://ciscobinary.openh264.org/x.bz2"));
    assert!(!codec_url_allowed("not a url"));
    assert!(codec_url_allowed("https://ciscobinary.openh264.org/x.bz2"));
}

/// A table entry the policy would refuse is a feature that cannot turn on, and nothing else
/// would catch it.
#[test]
fn every_target_url_passes_the_codec_policy() {
    for t in TARGETS {
        let url = t.url();
        assert!(codec_url_allowed(&url), "{url}");
    }
}

#[test]
fn the_codec_lives_in_its_own_directory_under_the_data_dir() {
    let dir = codec_dir();
    assert!(dir.ends_with("codec"), "{}", dir.display());
    assert!(
        dir.starts_with(crate::state::cache_data_dir()),
        "{} is not under the data dir",
        dir.display()
    );
}

#[test]
fn the_installed_path_is_the_targets_file_name_in_that_directory() {
    let target = current_target().unwrap();
    let path = installed_path(target);
    assert_eq!(path.file_name().unwrap(), target.file_name);
    assert_eq!(path.parent().unwrap(), codec_dir());
}

/// A known vector keeps the hashing and the hex encoding from drifting apart.
#[test]
fn the_digest_is_lowercase_hex_sha256_of_the_plain_bytes() {
    assert_eq!(
        sha256_bytes(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn decompression_returns_the_original_bytes() {
    use std::io::Write;
    let plain = b"openh264 stand-in payload".repeat(64);
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(&plain).unwrap();
    let packed = encoder.finish().unwrap();

    assert_eq!(decompress_capped(&packed, 1 << 20).unwrap(), plain);
}

#[test]
fn decompression_refuses_to_exceed_the_ceiling() {
    use std::io::Write;
    let plain = vec![0u8; 4 * 1024 * 1024];
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(&plain).unwrap();
    let packed = encoder.finish().unwrap();

    let err = decompress_capped(&packed, 1024).unwrap_err();
    assert!(err.to_string().contains("ceiling"), "{err}");
}

#[test]
fn a_truncated_archive_is_an_error_not_a_short_read() {
    use std::io::Write;
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(b"payload").unwrap();
    let packed = encoder.finish().unwrap();

    assert!(decompress_capped(&packed[..packed.len() / 2], 1 << 20).is_err());
}

#[test]
fn every_status_variant_names_a_distinct_reason() {
    let reasons = [
        CodecStatus::Disabled.reason(),
        CodecStatus::NotDownloaded.reason(),
        CodecStatus::DownloadFailed(String::new()).reason(),
        CodecStatus::DigestMismatch.reason(),
        CodecStatus::Unsupported.reason(),
    ];
    for (i, a) in reasons.iter().enumerate() {
        assert!(!a.is_empty());
        for b in &reasons[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

/// `every_status_variant_names_a_distinct_reason` would still pass with the cause dropped.
#[test]
fn download_failed_reason_carries_the_cause() {
    let reason = CodecStatus::DownloadFailed("connection reset by peer".to_string()).reason();
    assert!(reason.contains("connection reset by peer"), "{reason}");
}

#[test]
fn a_ready_status_is_the_only_one_that_yields_a_path() {
    let ready = CodecStatus::Ready(std::path::PathBuf::from("/tmp/x"));
    assert!(ready.path().is_some());
    assert!(CodecStatus::Disabled.path().is_none());
    assert!(CodecStatus::NotDownloaded.path().is_none());
    assert!(CodecStatus::DownloadFailed("boom".into()).path().is_none());
    assert!(CodecStatus::DigestMismatch.path().is_none());
    assert!(CodecStatus::Unsupported.path().is_none());
}

/// Only the digest rejection is covered: an unknown file fails `from_blob_path` before the
/// probe. Reaching the probe needs a genuine Cisco digest on a file without Wels symbols, a
/// manual-validation item.
#[test]
fn a_blob_with_an_unknown_digest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-codec.so");
    std::fs::write(&path, b"this is not a shared library").unwrap();

    assert!(load_api(&path).is_err());
}

#[test]
fn a_file_whose_digest_does_not_match_the_target_is_a_digest_mismatch_and_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wrong-bytes.so");
    std::fs::write(&path, b"these are not the bytes the digest was taken over").unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "not-a-digest-that-will-ever-match",
        file_name: "wrong-bytes.so",
    };

    let status = verify_installed(&path, &target);

    assert!(matches!(status, CodecStatus::DigestMismatch));
    assert!(
        !path.exists(),
        "the mismatched file should have been deleted"
    );
}

#[test]
fn a_missing_file_is_not_downloaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never-written.so");
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "irrelevant-because-the-file-is-never-read",
        file_name: "never-written.so",
    };

    let status = verify_installed(&path, &target);

    assert!(matches!(status, CodecStatus::NotDownloaded));
}

#[test]
fn a_digest_mismatch_is_still_classified_after_being_wrapped_in_context() {
    let err: anyhow::Error = CodecDigestMismatch {
        expected: "aaaa",
        found: "bbbb".to_string(),
    }
    .into();
    let wrapped = err.context("fetching the codec failed");

    assert!(matches!(
        install_failure(&wrapped),
        CodecStatus::DigestMismatch
    ));
}

#[test]
fn any_other_install_error_is_a_download_failure_carrying_its_text() {
    let err = anyhow::anyhow!("codec host answered 503");

    assert!(matches!(
        install_failure(&err),
        CodecStatus::DownloadFailed(cause) if cause == "codec host answered 503"
    ));
}
