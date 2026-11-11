//! Tests for `src/ipc/plugin/download/mod.rs`, attached to it by `#[path]`.

use super::*;

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

/// Under a folder grant nobody sees the name a plugin composes. A FLAC passes the content check
/// whatever it is called, and its comments are plugin text: as `x.bat` a shell would run them.
#[tokio::test]
async fn a_download_named_for_another_program_is_refused_before_anything_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    crate::ui::file_dialog::record_folder_grant(dir.path()).expect("the folder is granted");

    let err = run(request_into(&dir.path().join("x.bat")), None)
        .await
        .expect_err("a non-audio name is refused");

    assert!(format!("{err:#}").contains(NOT_AUDIO_NAME), "{err:#}");
    assert_eq!(status_for(&err), 403);
    assert!(!dir.path().join("x.bat").exists());
}
