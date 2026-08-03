//! Tests for `src/ipc/plugin/download.rs`, attached to it by `#[path]`.

use super::*;

/// Separators are already directory boundaries by the time this sees them, so only the
/// last segment is cleaned, as upstream's `path.join`, which likewise cannot tell a
/// caller's separator from one out of a track title.
#[test]
fn an_already_split_path_keeps_its_directories() {
    let out = sanitized_destination("/music/AC/DC - Song.flac").unwrap();
    assert_eq!(out, std::path::Path::new("/music/AC/DC - Song.flac"));
}

#[test]
fn characters_windows_refuses_are_replaced() {
    let out = sanitized_destination("/music/A: B? C* D\" E<F>G|H.flac").unwrap();
    assert_eq!(out.file_name().unwrap(), "A_ B_ C_ D_ E_F_G_H.flac");
}

#[test]
fn a_trailing_dot_or_space_is_removed() {
    assert_eq!(
        sanitized_destination("/m/Song. ")
            .unwrap()
            .file_name()
            .unwrap(),
        "Song"
    );
}

#[test]
fn a_reserved_windows_stem_is_prefixed() {
    assert_eq!(
        sanitized_destination("/m/aux.flac")
            .unwrap()
            .file_name()
            .unwrap(),
        "_aux.flac"
    );
}

#[test]
fn a_name_that_sanitizes_to_nothing_is_refused() {
    assert!(sanitized_destination("/m/...").is_err());
    assert!(sanitized_destination("/").is_err());
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

#[test]
fn an_over_long_name_is_bounded_and_keeps_its_extension() {
    let out = sanitized_name(&format!("/m/{}.flac", "a".repeat(400)));
    assert!(out.len() <= MAX_NAME_BYTES, "{} bytes", out.len());
    assert!(out.ends_with(".flac"));
}

/// A byte budget, not a character count: 300 CJK characters are 900 bytes, so a cut that
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
fn genres_becomes_the_singular_field_players_read() {
    assert_eq!(vorbis_field("genres"), "GENRE");
    assert_eq!(vorbis_field("albumArtist"), "ALBUMARTIST");
    assert_eq!(vorbis_field("musicbrainz_trackid"), "MUSICBRAINZ_TRACKID");
}

#[test]
fn field_names_outside_printable_ascii_are_refused() {
    assert!(valid_field_name("TITLE"));
    assert!(!valid_field_name(""));
    assert!(!valid_field_name("TITLE=X"));
    assert!(!valid_field_name("TIT\nLE"));
    // to_uppercase is Unicode aware, so a non-ASCII key can reach here expanded.
    assert!(!valid_field_name("TITLÉ"));
}

/// The regression that shipped twice: the dialog recorded its raw answer while the download
/// resolved the sanitised name; every destination sanitisation touched was refused. The two
/// forms have to be the same form, and no test paired them.
#[test]
fn the_recorded_form_is_the_sanitised_one() {
    let dir = tempfile::tempdir().unwrap();
    // `:` is legal on Linux and sanitisation rewrites it: the two forms differ here.
    let answered = dir.path().join("Song: Live.flac");
    let dest = sanitized_destination(answered.to_str().unwrap()).unwrap();
    assert_ne!(answered, dest, "the fixture must exercise a rename");

    crate::ui::file_dialog::record_user_choice(&answered, false);
    assert!(
        crate::ui::file_dialog::authorisation_for(&dest).is_none(),
        "recording the raw answer authorises nothing"
    );

    crate::ui::file_dialog::record_user_choice(&dest, false);
    assert!(crate::ui::file_dialog::authorisation_for(&dest).is_some());
}

/// The user was asked about this exact file and said replace. It is replaced.
#[tokio::test]
async fn a_confirmed_replacement_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"stale").unwrap();

    let before = FileIdentity::of(&dest);
    write_file(dest.clone(), b"fresh".to_vec(), OnExisting::Replace, before)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
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

    let result = write_file(dest.clone(), b"fresh".to_vec(), OnExisting::Replace, before).await;

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

    let result = write_file(dest.clone(), b"fresh".to_vec(), OnExisting::Replace, before).await;

    assert!(result.is_err(), "an arrival must not be replaced");
    assert_eq!(std::fs::read(&dest).unwrap(), b"arrived meanwhile");
}

/// And a destination that stayed empty still writes. The check does not turn every first-time
/// download into a refusal.
#[tokio::test]
async fn an_untouched_empty_destination_still_writes() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");

    let before = FileIdentity::of(&dest);
    write_file(dest.clone(), b"fresh".to_vec(), OnExisting::Replace, before)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
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
    let result = write_file(dest.clone(), b"fresh".to_vec(), OnExisting::Fail, before).await;

    assert!(result.is_err(), "an unconfirmed replacement must fail");
    assert_eq!(std::fs::read(&dest).unwrap(), b"appeared meanwhile");
}

/// A FLAC signature, a STREAMINFO block marked last, then frame bytes.
fn flac(frame_bytes: usize) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.push(0x80); // last block, type 0 = STREAMINFO
    out.extend_from_slice(&[0x00, 0x00, 0x22]); // 34, STREAMINFO's fixed length
    out.extend_from_slice(&[0u8; 34]);
    out.extend(std::iter::repeat_n(0xAAu8, frame_bytes));
    out
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

/// Both containers TIDAL actually serves over BTS. Rejecting `ftyp` is what broke an earlier
/// attempt: AAC arrives as ISO-BMFF, not raw ADTS; refusing it fails every AAC track.
#[test]
fn the_containers_tidal_serves_are_accepted() {
    assert!(is_audio_container(&flac(64)), "FLAC");
    assert!(is_audio_container(&mp4(64)), "ISO-BMFF/M4A");
}

/// A DASH download is the init segment followed by its media segments, and `fetch_track` already
/// concatenates a url list. The only question is which manifests are allowed and how many urls.
#[test]
fn both_manifest_types_are_accepted_and_others_are_not() {
    assert!(is_supported_manifest(BTS_MIME));
    assert!(is_supported_manifest(DASH_MIME));
    assert!(!is_supported_manifest("audio/mpeg"));
    assert!(!is_supported_manifest(""));
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

/// A concatenated fMP4 is `ftyp`+`moov` then one `moof`+`mdat` per segment; the box chain accounts
/// for every byte and `is_audio_container` accepts it. This is what makes the ISO-BMFF branch reachable.
#[test]
fn a_concatenated_dash_body_is_a_valid_container() {
    let mut body = box_of(b"ftyp", 16);
    body.extend(box_of(b"moov", 40));
    for _ in 0..3 {
        body.extend(box_of(b"moof", 24));
        body.extend(box_of(b"mdat", 512));
    }
    assert!(is_audio_container(&body));
}

/// A zero-size box means "to end of file": one would swallow any payload while the walk still
/// terminated happily (the accounting property itself).
#[test]
fn a_zero_size_box_cannot_swallow_a_payload() {
    let mut polyglot = box_of(b"ftyp", 16);
    polyglot.extend_from_slice(&0u32.to_be_bytes());
    polyglot.extend_from_slice(b"mdat");
    polyglot.extend_from_slice(b"PK\x03\x04 an archive read from its tail");
    assert!(!is_audio_container(&polyglot));
}

/// A truncated final segment leaves a box that overruns the body, which the chain walk catches; a
/// partial DASH download cannot be written as if it were whole.
#[test]
fn a_truncated_dash_body_is_refused() {
    let mut body = box_of(b"ftyp", 16);
    body.extend(box_of(b"moov", 40));
    let mut cut = box_of(b"mdat", 512);
    cut.truncate(cut.len() - 100);
    body.extend(cut);
    assert!(!is_audio_container(&body));
}

/// One ISO-BMFF box: a big-endian total size, the type, then payload.
fn box_of(kind: &[u8; 4], total: usize) -> Vec<u8> {
    let mut out = (total as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend(std::iter::repeat_n(0u8, total - 8));
    out
}

/// Each puts `ftyp` at offset 4 with four bytes of its own comment syntax, and each is a file another
/// program acts on.
#[test]
fn a_polyglot_that_merely_places_the_magic_is_refused() {
    let mut desktop = b"#abcftyp\n".to_vec();
    desktop.extend_from_slice(b"[Desktop Entry]\nType=Application\nExec=rm -rf /\n");
    assert!(!is_audio_container(&desktop), "desktop entry");

    let mut xml = b"<!--ftyp-->".to_vec();
    xml.extend_from_slice(b"<?xml version=\"1.0\"?><plist><dict/></plist>");
    assert!(!is_audio_container(&xml), "xml/plist");
}

/// Bytes glued on after the chain fall outside every box. The walk lands past the end and refuses.
/// This is the one polyglot shape ISO-BMFF catches and FLAC does not; it is not a seal (see
/// `a_sized_box_may_carry_a_foreign_payload`).
#[test]
fn data_appended_to_an_iso_bmff_body_breaks_the_chain() {
    let mut with_zip = mp4(64);
    with_zip.extend_from_slice(b"PK\x03\x04 appended archive");
    assert!(!is_audio_container(&with_zip));
}

/// The ceiling this check actually buys, pinned here to avoid being mistaken for a seal: a box whose declared
/// size accounts for a whole archive is accepted, and a ZIP reader still finds its directory by
/// scanning back from the end, prefix and all. Closing this means demuxing `mdat`, not a tighter walk.
#[test]
fn a_sized_box_may_carry_a_foreign_payload() {
    let zip = b"PK\x03\x04 a whole archive, found from its tail PK\x05\x06";
    let mut carried = box_of(b"ftyp", 16);
    carried.extend_from_slice(&((zip.len() + 8) as u32).to_be_bytes());
    carried.extend_from_slice(b"mdat");
    carried.extend_from_slice(zip);
    assert!(
        is_audio_container(&carried),
        "accepted by design: the payload region is never inspected"
    );
}

/// The same ceiling on the FLAC side: everything past the metadata chain is frame bytes as far as
/// this can tell; the two branches carry the same risk rather than one covering for the other.
#[test]
fn a_flac_frame_region_may_carry_a_foreign_payload() {
    let mut carried = flac(0);
    carried.extend_from_slice(b"PK\x03\x04 a whole archive, found from its tail PK\x05\x06");
    assert!(is_audio_container(&carried));
}

#[test]
fn a_body_whose_boxes_run_past_the_end_is_refused() {
    let mut lying = 9999u32.to_be_bytes().to_vec();
    lying.extend_from_slice(b"ftypM4A ");
    lying.extend_from_slice(&[0u8; 16]);
    assert!(!is_audio_container(&lying));
}

/// A signature with no stream behind it is not a track, and a block header overrunning the body
/// describes something else.
#[test]
fn a_flac_header_without_frames_or_with_a_lying_block_is_refused() {
    let mut header_only = b"fLaC".to_vec();
    header_only.push(0x80);
    header_only.extend_from_slice(&[0x00, 0x00, 0x22]);
    header_only.extend_from_slice(&[0u8; 34]);
    assert!(!is_audio_container(&header_only), "no frames");

    let mut lying = b"fLaC".to_vec();
    lying.push(0x80);
    lying.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    lying.extend_from_slice(&[0u8; 34]);
    assert!(!is_audio_container(&lying), "block runs past the end");
}

/// STREAMINFO opens the chain in every real FLAC stream, at a fixed length.
#[test]
fn a_flac_chain_not_opening_on_streaminfo_is_refused() {
    let mut wrong_type = b"fLaC".to_vec();
    wrong_type.push(0x84); // last block, type 4 = VORBIS_COMMENT
    wrong_type.extend_from_slice(&[0x00, 0x00, 0x22]);
    wrong_type.extend_from_slice(&[0u8; 40]);
    assert!(!is_audio_container(&wrong_type));
}

/// The point of the check. None of these can begin with an audio container's magic. A
/// destination the user was tricked into accepting cannot receive a file another program would
/// act on.
#[test]
fn formats_another_program_would_execute_are_refused() {
    assert!(!is_audio_container(b"[Desktop Entry]\nExec=rm -rf /\n"));
    assert!(!is_audio_container(b"#!/bin/sh\nrm -rf /\n"));
    assert!(!is_audio_container(b"MZ\x90\x00\x03\x00\x00\x00\x04\x00"));
    assert!(!is_audio_container(b"<?xml version=\"1.0\"?><plist/>"));
    assert!(!is_audio_container(b"\x7fELF\x02\x01\x01\x00\x00\x00"));
}

#[test]
fn a_body_too_short_to_carry_a_container_is_refused() {
    assert!(!is_audio_container(b""));
    assert!(!is_audio_container(b"fLa"));
    // Four bytes of magic and nothing else: no stream can follow; this is not a track.
    assert!(!is_audio_container(b"fLaC"));
}

/// `ftyp` sits at offset 4, after the box size. A body carrying it anywhere else is not
/// ISO-BMFF and must not pass by containing the word.
#[test]
fn ftyp_is_only_accepted_where_iso_bmff_puts_it() {
    let mut misplaced = b"ftyp".to_vec();
    misplaced.extend_from_slice(&[0u8; 34]);
    assert!(!is_audio_container(&misplaced), "ftyp at offset 0");

    let mut buried = vec![0u8; 40];
    buried[20..24].copy_from_slice(b"ftyp");
    assert!(!is_audio_container(&buried), "ftyp deeper in the body");
}

/// The url is plugin-supplied and `verr!` is never level-gated: logging it verbatim let a
/// refused-call loop write attacker-chosen text (newlines, terminal escapes) into the persistent
/// log at LOGS=0. Only the host is kept, and only bounded.
#[test]
fn a_refused_url_is_reduced_to_a_bounded_host_for_the_log() {
    let forged = "https://evil.test/x\n2026-08-01 [AUTH] token=deadbeef";
    let logged = refused_host(forged);
    assert_eq!(logged, "evil.test");
    assert!(!logged.contains('\n'));

    let long = format!("https://{}.test/x", "a".repeat(300));
    assert!(refused_host(&long).len() <= 64);

    assert_eq!(refused_host("not a url"), "unparseable url");
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
    let outcome = write_file(dest.clone(), b"ours".to_vec(), OnExisting::Skip, None)
        .await
        .expect("a folder grant must not fail on an existing file");

    assert!(matches!(outcome, WriteOutcome::Skipped));
    assert_eq!(std::fs::read(&dest).unwrap(), b"arrived mid-download");
}

/// A request admitted a moment before the run's deadline used to get its own full `TRACK_TIMEOUT`,
/// A run could then hold the single download permit for half again the stated hour. The per-request
/// timeout is the remaining deadline once that is the smaller of the two.
#[test]
fn a_request_never_outlives_the_runs_deadline() {
    assert_eq!(
        next_request_timeout(Duration::from_secs(5)),
        Some(Duration::from_secs(5)),
        "near the deadline, the run's remainder wins"
    );
    assert_eq!(
        next_request_timeout(TRACK_TIMEOUT * 2),
        Some(TRACK_TIMEOUT),
        "with time to spare, the per-request limit still applies"
    );
    assert_eq!(
        next_request_timeout(TRACK_TIMEOUT),
        Some(TRACK_TIMEOUT),
        "equal is not over"
    );
    assert!(
        next_request_timeout(Duration::ZERO).is_none(),
        "a spent deadline admits no further request"
    );
}

/// The hosts TIDAL actually served in a capture: `lgf.audio.tidal.com` for BTS, `sp-ad-fa.audio
/// .tidal.com` for DASH segments.
#[test]
fn tidal_media_hosts_are_accepted() {
    assert!(is_tidal_media_host("lgf.audio.tidal.com"));
    assert!(is_tidal_media_host("sp-ad-fa.audio.tidal.com"));
    assert!(is_tidal_media_host("audio.tidal.com"));
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

/// Userinfo is the classic way past a check that reads the string rather than the parsed host: this
/// url's host is `evil.com`.
#[test]
fn a_userinfo_prefix_does_not_impersonate_a_host() {
    assert!(!allowed_url(
        "https://lgf.audio.tidal.com@evil.com/track.flac",
        is_tidal_media_host
    ));
}

/// Plaintext would hand the stream to anyone on the path, and TIDAL serves none of this over http.
#[test]
fn plaintext_is_refused() {
    assert!(!allowed_url(
        "http://lgf.audio.tidal.com/track.flac",
        is_tidal_media_host
    ));
    assert!(allowed_url(
        "https://lgf.audio.tidal.com/track.flac",
        is_tidal_media_host
    ));
}

#[test]
fn an_unparseable_url_is_refused() {
    assert!(!allowed_url("not a url", is_tidal_media_host));
    assert!(!allowed_url("", is_tidal_media_host));
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
