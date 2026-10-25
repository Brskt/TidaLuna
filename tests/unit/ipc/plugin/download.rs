//! Tests for `src/ipc/plugin/download.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn a_destination_name_is_made_safe_for_every_filesystem_or_refused() {
    for (path, expected) in [
        (
            "/music/A: B? C* D\" E<F>G|H.flac",
            Some("A_ B_ C_ D_ E_F_G_H.flac"),
        ),
        ("/m/Song. ", Some("Song")),
        ("/m/aux.flac", Some("_aux.flac")),
        ("/m/...", None),
        ("/", None),
    ] {
        match expected {
            Some(name) => assert_eq!(sanitized_name(path), name, "{path:?}"),
            None => assert!(sanitized_destination(path).is_err(), "{path:?}"),
        }
    }
}

fn sanitized_name(path: &str) -> String {
    sanitized_destination(path)
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

/// A byte budget, not a character count: 300 CJK characters are 900 bytes; a cut that
/// counted characters would still hand the filesystem a name it refuses.
#[test]
fn a_multibyte_name_is_bounded_in_bytes_on_a_char_boundary() {
    let out = sanitized_name(&format!("/m/{}.flac", "曲".repeat(300)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
    assert!(out.ends_with(".flac"));
    // 250 bytes of stem budget is 83 whole characters, not 83 and a third.
    assert_eq!(out.chars().filter(|c| *c == '曲').count(), 83);
}

#[test]
fn an_extension_filling_the_budget_leaves_no_stem_to_keep() {
    let out = sanitized_name(&format!("/m/s.{}", "e".repeat(400)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
}

#[test]
fn a_trailing_space_exposed_by_the_cut_is_removed() {
    let out = sanitized_name(&format!("/m/{} {}", "a".repeat(254), "b".repeat(50)));
    assert_eq!(out.len(), 254);
    assert!(!out.ends_with(' '));
}

#[test]
fn field_names_outside_printable_ascii_are_refused() {
    assert!(valid_field_name("TITLE"));
    assert!(!valid_field_name(""));
    assert!(!valid_field_name("TITLE=X"));
    assert!(!valid_field_name("TIT\nLE"));
    // to_uppercase is Unicode aware: a non-ASCII key can reach here expanded.
    assert!(!valid_field_name("TITLÉ"));
}

/// The consent names a file, not a path, and the fetch between the two can run for an hour. What is
/// at the destination now is not what the run was authorised against: replacing it would destroy
/// something nobody was ever asked about, and `persist` renames over whatever it finds.
#[tokio::test]
async fn a_destination_swapped_while_downloading_is_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"the file the user was asked about").unwrap();

    let before = FileIdentity::of(&dest);

    std::fs::remove_file(&dest).unwrap();
    std::fs::write(&dest, b"someone else's file").unwrap();

    let result = write_file(
        dest.clone(),
        b"fresh".to_vec(),
        OnExisting::Replace,
        before,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "a swapped destination must not be replaced"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"someone else's file");
}

/// The same rule from the other side: nothing was there when the run started; a file that turned
/// up while it downloaded was never confirmed either.
#[tokio::test]
async fn a_file_arriving_where_none_was_is_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");

    let before = FileIdentity::of(&dest);
    assert!(before.is_none(), "nothing is at the destination yet");

    std::fs::write(&dest, b"arrived meanwhile").unwrap();

    let result = write_file(
        dest.clone(),
        b"fresh".to_vec(),
        OnExisting::Replace,
        before,
        None,
    )
    .await;

    assert!(result.is_err(), "an arrival must not be replaced");
    assert_eq!(std::fs::read(&dest).unwrap(), b"arrived meanwhile");
}

/// The other half of the rule. A file that turned up after the user answered, or one the
/// dialog never named because sanitisation renamed it, was never confirmed. Refusing is the
/// point: one earlier version destroyed it, and the one before that reported success while
/// leaving it untouched.
#[tokio::test]
async fn an_unconfirmed_destination_is_neither_replaced_nor_called_written() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"appeared meanwhile").unwrap();

    let before = FileIdentity::of(&dest);
    let result = write_file(
        dest.clone(),
        b"fresh".to_vec(),
        OnExisting::Fail,
        before,
        None,
    )
    .await;

    assert!(result.is_err(), "an unconfirmed replacement must fail");
    assert_eq!(std::fs::read(&dest).unwrap(), b"appeared meanwhile");
}

/// An `ftyp` box followed by one box that accounts for the rest.
fn mp4(payload: usize) -> Vec<u8> {
    let mut out = 24u32.to_be_bytes().to_vec();
    out.extend_from_slice(b"ftypM4A ");
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend(std::iter::repeat_n(0xAAu8, payload));
    out
}

/// The url list comes from the plugin. An unbounded one would have Rust issue as many requests as
/// it likes. The byte ceiling alone does not stop that: a list of tiny 404s costs nothing per url.
#[test]
fn a_url_list_longer_than_a_real_manifest_is_refused() {
    assert!(url_count_within_bounds(1));
    assert!(url_count_within_bounds(MAX_URLS));
    assert!(!url_count_within_bounds(MAX_URLS + 1));
    assert!(
        !url_count_within_bounds(0),
        "an empty list has nothing to fetch"
    );
}

/// One ISO-BMFF box: a big-endian total size, the type, then payload.
fn box_of(kind: &[u8; 4], total: usize) -> Vec<u8> {
    let mut out = (total as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend(std::iter::repeat_n(0u8, total - 8));
    out
}

/// Bodies that only dress up as an audio container, each a file another program would act on or
/// a download cut short. Not a seal: a box sized to hold a whole archive still passes.
#[test]
fn a_body_that_only_dresses_up_as_an_audio_container_is_refused() {
    // A zero-size box means "to end of file" and would swallow any payload.
    let mut zero_size_box = box_of(b"ftyp", 16);
    zero_size_box.extend_from_slice(&0u32.to_be_bytes());
    zero_size_box.extend_from_slice(b"mdat");
    zero_size_box.extend_from_slice(b"PK\x03\x04 an archive read from its tail");

    let mut desktop = b"#abcftyp\n".to_vec();
    desktop.extend_from_slice(b"[Desktop Entry]\nType=Application\nExec=rm -rf /\n");

    let mut xml = b"<!--ftyp-->".to_vec();
    xml.extend_from_slice(b"<?xml version=\"1.0\"?><plist><dict/></plist>");

    let mut truncated = box_of(b"ftyp", 16);
    truncated.extend(box_of(b"moov", 40));
    let mut cut = box_of(b"mdat", 512);
    cut.truncate(cut.len() - 100);
    truncated.extend(cut);

    let mut with_zip = mp4(64);
    with_zip.extend_from_slice(b"PK\x03\x04 appended archive");

    let mut overrunning = 9999u32.to_be_bytes().to_vec();
    overrunning.extend_from_slice(b"ftypM4A ");
    overrunning.extend_from_slice(&[0u8; 16]);

    let mut flac_header_only = b"fLaC".to_vec();
    flac_header_only.push(0x80);
    flac_header_only.extend_from_slice(&[0x00, 0x00, 0x22]);
    flac_header_only.extend_from_slice(&[0u8; 34]);

    let mut flac_lying_block = b"fLaC".to_vec();
    flac_lying_block.push(0x80);
    flac_lying_block.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    flac_lying_block.extend_from_slice(&[0u8; 34]);

    let mut flac_not_streaminfo = b"fLaC".to_vec();
    flac_not_streaminfo.push(0x84); // last block, type 4 = VORBIS_COMMENT
    flac_not_streaminfo.extend_from_slice(&[0x00, 0x00, 0x22]);
    flac_not_streaminfo.extend_from_slice(&[0u8; 40]);

    // `ftyp` sits at offset 4, after the box size: carried anywhere else it is not ISO-BMFF.
    let mut ftyp_first = b"ftyp".to_vec();
    ftyp_first.extend_from_slice(&[0u8; 34]);

    let mut ftyp_buried = vec![0u8; 40];
    ftyp_buried[20..24].copy_from_slice(b"ftyp");

    // Covers the body exactly but does not open on `ftyp`: only the magic's own check refuses it.
    let mut unopened = box_of(b"moov", 16);
    unopened.extend(box_of(b"mdat", 32));

    for (body, what) in [
        (zero_size_box, "a zero-size box swallowing a payload"),
        (desktop, "desktop entry"),
        (xml, "xml/plist"),
        (truncated, "truncated DASH body"),
        (with_zip, "data appended after the chain"),
        (overrunning, "a box running past the end"),
        (flac_header_only, "no frames"),
        (flac_lying_block, "block runs past the end"),
        (flac_not_streaminfo, "a chain opening on VORBIS_COMMENT"),
        (
            b"[Desktop Entry]\nExec=rm -rf /\n".to_vec(),
            "a desktop entry",
        ),
        (b"#!/bin/sh\nrm -rf /\n".to_vec(), "a shell script"),
        (
            b"MZ\x90\x00\x03\x00\x00\x00\x04\x00".to_vec(),
            "a PE header",
        ),
        (b"<?xml version=\"1.0\"?><plist/>".to_vec(), "a bare plist"),
        (b"\x7fELF\x02\x01\x01\x00\x00\x00".to_vec(), "an ELF header"),
        (ftyp_first, "ftyp at offset 0"),
        (ftyp_buried, "ftyp deeper in the body"),
        (unopened, "a chain with no ftyp first"),
        (Vec::new(), "an empty body"),
        (b"fLa".to_vec(), "a truncated FLAC magic"),
        (b"fLaC".to_vec(), "a FLAC magic and nothing else"),
    ] {
        assert!(!is_audio_container(&body), "{what}");
    }
}

/// A policy refusal is permanent. It must answer 403, not the 500 documented as worth a retry.
/// reqwest stores the redirect-policy error as a source rather than as itself, and `downcast_ref`
/// matches only the stored type; the classifier has to walk the chain.
#[test]
fn a_refusal_wrapped_by_another_error_still_answers_403() {
    let bare: anyhow::Error = Refused("refused").into();
    assert_eq!(status_for(&bare), 403);

    // Shaped like `reqwest::Error`, which holds the policy error and hands it back from `source()`
    // (reqwest-0.13.4/src/error.rs:288). Not `io::Error`: its `source()` delegates to the INNER
    // error's source; the inner error itself never appears in the chain.
    #[derive(Debug)]
    struct Carrier(Refused);
    impl std::fmt::Display for Carrier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error following redirect")
        }
    }
    impl std::error::Error for Carrier {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    let wrapped = anyhow::Error::new(Carrier(Refused("refused")));
    assert_eq!(
        status_for(&wrapped),
        403,
        "a refusal buried in the source chain is still a refusal"
    );

    let unrelated = anyhow::anyhow!("the write broke");
    assert_eq!(status_for(&unrelated), 500);
}

/// Nobody was asked about a file under a folder grant: one that appears while the track is
/// downloading is the same outcome as one that was there before: kept, and the track counts as done.
/// Returning `AlreadyExists` as a hard error contradicted the pre-fetch check three lines above it.
#[tokio::test]
async fn a_file_appearing_under_a_folder_grant_is_kept_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"arrived mid-download").unwrap();

    // `None`: nothing was there when the run started, which is what makes this an arrival.
    let outcome = write_file(dest.clone(), b"ours".to_vec(), OnExisting::Skip, None, None)
        .await
        .expect("a folder grant must not fail on an existing file");

    assert!(matches!(outcome, WriteOutcome::Skipped));
    assert_eq!(std::fs::read(&dest).unwrap(), b"arrived mid-download");
}

/// An install folder holding `frontend/`, beside the data dir as on Windows, and the guard built
/// over it.
fn guarded_install() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    &'static crate::platform::install_guard::Protected,
) {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("install");
    std::fs::create_dir_all(root.join("frontend")).unwrap();
    std::fs::write(root.join("frontend/native-host.cjs"), b"host").unwrap();
    let guard = Box::leak(Box::new(crate::platform::install_guard::Protected::new(
        &root,
        &parent.path().join("data/native"),
    )));
    (parent, root, guard)
}

/// A folder grant over the install folder would otherwise create files and directories inside
/// `frontend/`; nothing may be left behind, not a temp file, not a directory.
#[tokio::test]
async fn a_folder_grant_cannot_create_inside_frontend() {
    let (_parent, root, guard) = guarded_install();
    let dest = root.join("frontend/new/x.flac");

    let result = write_file(
        dest.clone(),
        b"bytes".to_vec(),
        OnExisting::Skip,
        None,
        Some(guard),
    )
    .await;

    assert!(result.is_err(), "a write inside frontend/ must be refused");
    assert!(
        !root.join("frontend/new").exists(),
        "no directory may be created"
    );
    let left: Vec<_> = std::fs::read_dir(root.join("frontend")).unwrap().collect();
    assert_eq!(left.len(), 1, "only native-host.cjs remains");
}

/// A save dialog the user confirmed does not open the app's own files to a plugin either.
#[tokio::test]
async fn a_confirmed_replace_inside_frontend_is_refused() {
    let (_parent, root, guard) = guarded_install();
    let dest = root.join("frontend/native-host.cjs");
    let before = FileIdentity::of(&dest);

    let result = write_file(
        dest.clone(),
        b"bytes".to_vec(),
        OnExisting::Replace,
        before,
        Some(guard),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"host");
}

/// A request for `dest` to a host that is not TIDAL's media host: the fetch refuses it, and any
/// other refusal came before it.
fn request_into(dest: &std::path::Path) -> Request {
    Request {
        manifest_mime_type: BTS_MIME.to_string(),
        urls: vec!["https://example.invalid/a".into()],
        key_id: String::new(),
        encryption_type: String::new(),
        path: dest.to_string_lossy().into_owned(),
        tags: None,
    }
}

/// A grant of the install's parent admits a sub-folder inside it; the guard refuses it before the
/// track is pulled, decrypted and retagged, not at the write.
#[tokio::test]
async fn a_download_into_the_install_folder_is_refused_before_anything_is_fetched() {
    let (parent, root, guard) = guarded_install();
    crate::ui::file_dialog::record_folder_grant(parent.path()).expect("the parent is granted");

    let err = run(request_into(&root.join("frontend/new/x.flac")), Some(guard))
        .await
        .expect_err("a download inside the install is refused");

    assert!(
        format!("{err:#}").contains("inside TidaLunar's own files"),
        "{err:#}"
    );
    assert_eq!(status_for(&err), 403);
    assert!(!root.join("frontend/new").exists());
}

/// A file already in the install answered `Ok` under a folder grant, an absent one 403: the reply
/// told a plugin which of the app's files exist.
#[tokio::test]
async fn an_existing_install_file_is_refused_like_an_absent_one() {
    let (parent, root, guard) = guarded_install();
    crate::ui::file_dialog::record_folder_grant(parent.path()).expect("the parent is granted");

    let err = run(
        request_into(&root.join("frontend/native-host.cjs")),
        Some(guard),
    )
    .await
    .expect_err("an existing file inside the install is refused");

    assert!(
        format!("{err:#}").contains("inside TidaLunar's own files"),
        "{err:#}"
    );
}

/// A substring test accepts all three of these, and each is a host someone else can control.
#[test]
fn lookalike_hosts_are_refused() {
    assert!(!is_tidal_media_host("evil-audio.tidal.com"));
    assert!(!is_tidal_media_host("audio.tidal.com.evil.com"));
    assert!(!is_tidal_media_host("evil.com"));
    assert!(!is_tidal_media_host(""));
}

#[test]
fn artwork_comes_from_its_own_host_only() {
    assert!(is_tidal_artwork_host("resources.tidal.com"));
    assert!(!is_tidal_artwork_host("lgf.audio.tidal.com"));
    assert!(!is_tidal_artwork_host("resources.tidal.com.evil.com"));
}

/// Userinfo is the classic way past a check that reads the string rather than the parsed host (the
/// first url's host is `evil.com`), and plaintext would hand the stream to anyone on the path.
#[test]
fn only_an_https_url_whose_parsed_host_is_allowed_passes() {
    for (url, expected) in [
        ("https://lgf.audio.tidal.com@evil.com/track.flac", false),
        ("http://lgf.audio.tidal.com/track.flac", false),
        ("https://lgf.audio.tidal.com/track.flac", true),
        ("not a url", false),
        ("", false),
    ] {
        assert_eq!(allowed_url(url, is_tidal_media_host), expected, "{url:?}");
    }
}

/// Checking the submitted url is not enough on its own: the client follows redirects; an open
/// redirect on an allowed host would otherwise carry the fetch anywhere.
#[test]
fn a_redirect_leaving_the_allowed_hosts_is_refused() {
    let media = url::Url::parse("https://sp-ad-fa.audio.tidal.com/seg1.mp4").unwrap();
    let artwork = url::Url::parse("https://resources.tidal.com/cover.jpg").unwrap();
    let away = url::Url::parse("https://evil.com/seg1.mp4").unwrap();
    let plaintext = url::Url::parse("http://lgf.audio.tidal.com/track.flac").unwrap();

    assert!(redirect_allowed(&media));
    assert!(
        redirect_allowed(&artwork),
        "media and artwork share one client"
    );
    assert!(!redirect_allowed(&away));
    assert!(!redirect_allowed(&plaintext));
}
