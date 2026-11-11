//! Tests for `src/video/codec/install.rs`, attached by `#[path]`.

use super::*;

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

#[test]
fn a_host_other_than_the_codec_host_is_refused() {
    for host in [
        "ciscobinary.openh264.org.evil.com",
        "evil-ciscobinary.openh264.org",
        "openh264.org",
        "sub.ciscobinary.openh264.org",
        "audio.tidal.com",
        "resources.tidal.com",
        "desktop.tidal.com",
    ] {
        assert!(!is_codec_host(host), "{host}");
    }
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

/// A file that is no shared library is refused, by its digest or by the loader. Reaching the
/// probe needs a genuine Cisco digest on a file without Wels symbols, a manual-validation item.
#[test]
fn a_file_that_is_not_a_library_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-codec.so");
    std::fs::write(&path, b"this is not a shared library").unwrap();

    assert!(load_api(&path).is_err());
}

#[test]
fn a_file_whose_digest_does_not_match_the_target_is_removed_and_left_to_download() {
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

    assert!(matches!(status, Verified::Removed));
    assert!(
        !path.exists(),
        "the mismatched file should have been deleted"
    );
}

/// A failing removal stands in for a library Windows keeps loaded: the file reads, and removing
/// it does not succeed.
#[test]
fn a_mismatched_file_that_cannot_be_removed_is_stuck() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stuck-bytes.so");
    std::fs::write(&path, b"these are not the bytes the digest was taken over").unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "not-a-digest-that-will-ever-match",
        file_name: "stuck-bytes.so",
    };

    let status = verify_installed_with(&path, &target, |_| {
        Err(std::io::ErrorKind::PermissionDenied.into())
    });

    assert!(matches!(status, Verified::Stuck(_)));
    assert!(path.exists(), "the removal was refused");
}

#[test]
fn a_file_that_cannot_be_read_is_stuck_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a-directory.so");
    std::fs::create_dir(&path).unwrap();
    let target = CodecTarget {
        os: "test",
        arch: "test",
        sha256: "irrelevant-because-the-file-cannot-be-read",
        file_name: "a-directory.so",
    };

    assert!(matches!(
        verify_installed(&path, &target),
        Verified::Stuck(_)
    ));
}

/// Binds a listener that answers its first request with `response` verbatim, then drops the
/// socket.
async fn answering_port(response: &'static [u8]) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock.write_all(response).await;
        let _ = sock.flush().await;
    });
    port
}

/// The redirect policy refuses through the same `send()` error as an unreachable host.
#[tokio::test]
async fn a_redirect_off_the_codec_host_is_a_refusal() {
    let port = answering_port(
        b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/x.bz2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;

    let e = download_archive(&format!("http://127.0.0.1:{port}/x.bz2"))
        .await
        .unwrap_err();

    assert!(matches!(e, FetchError::Redirected(_)), "{e}");
    assert_eq!(e.class(), Some(FailureClass::Refused));
}

/// The codec retries a `Network` host on a timer, and a cover page asks again without spending:
/// a "no" read as "not now" retries forever, a "not now" read as "no" gives up on a host that
/// would have served.
#[test]
fn a_host_status_is_not_now_only_when_it_says_so() {
    for (status, class) in [
        (500, FailureClass::Network),
        (408, FailureClass::Network),
        (429, FailureClass::Network),
        (404, FailureClass::Refused),
    ] {
        let status = reqwest::StatusCode::from_u16(status).unwrap();
        assert_eq!(FailureClass::of_status(status), class, "{status}");
    }
}
