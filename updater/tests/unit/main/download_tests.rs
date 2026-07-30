//! Tests for `updater/src/main.rs`, attached to it by `#[path]`.

use std::io::Cursor;

use super::{read_capped, stream_to_file};

#[test]
fn read_capped_returns_body_under_cap() {
    let body = vec![3u8; 100];
    assert_eq!(read_capped(Cursor::new(body.clone()), 1000).unwrap(), body);
}

#[test]
fn read_capped_allows_exactly_at_cap() {
    let body = vec![5u8; 64];
    assert_eq!(read_capped(Cursor::new(body.clone()), 64).unwrap(), body);
}

#[test]
fn read_capped_rejects_over_cap() {
    let err = read_capped(Cursor::new(vec![0u8; 65]), 64).unwrap_err();
    assert!(err.to_string().contains("cap"), "got: {err}");
}

#[test]
fn stream_to_file_writes_body_under_cap() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let body = vec![7u8; 100];
    stream_to_file(Cursor::new(body.clone()), &dest, 1000).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[test]
fn stream_to_file_accepts_body_exactly_at_cap() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let body = vec![1u8; 64];
    stream_to_file(Cursor::new(body.clone()), &dest, 64).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[test]
fn stream_to_file_rejects_body_over_cap() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let body = vec![0u8; 65];
    let err = stream_to_file(Cursor::new(body), &dest, 64).unwrap_err();
    assert!(err.to_string().contains("cap"), "got: {err}");
    assert!(
        !dest.exists(),
        "partial file must be removed when the cap is exceeded"
    );
}
