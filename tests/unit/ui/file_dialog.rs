//! Tests for `src/ui/file_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// Matching does not consume: a fetch that fails after the check must not cost the user their
/// answer.
#[test]
fn an_authorisation_survives_until_it_is_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let chosen = dir.path().join("song.flac");
    record_user_choice(&chosen, false).unwrap();

    assert!(authorisation_for(&chosen).is_some(), "checked once");
    let auth = authorisation_for(&chosen).expect("a failed attempt must not burn it");

    consume(auth);
    assert!(
        authorisation_for(&chosen).is_none(),
        "one dialog answer buys one written file"
    );
}

/// Refused even where it lands back on the authorised file. Resolving `..` to accept this is what the
/// platform split rules out, and no dialog answer needs it.
#[test]
fn a_traversal_that_lands_on_the_authorised_file_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let chosen = inner.join("song.flac");
    record_user_choice(&chosen, false).unwrap();

    assert!(authorisation_for(&inner.join("..").join("inner").join("song.flac")).is_none());
    assert!(
        authorisation_for(&chosen).is_some(),
        "the real one survived the attempt"
    );
}

/// Only the dialog handler knows whether the OS asked about replacing this exact name. The
/// answer travels with the entry rather than being guessed from disk at write time.
#[test]
fn whether_replacement_was_confirmed_travels_with_the_entry() {
    let dir = tempfile::tempdir().unwrap();

    let unconfirmed = dir.path().join("new.flac");
    record_user_choice(&unconfirmed, false).unwrap();
    assert!(!authorisation_for(&unconfirmed).unwrap().replace_confirmed());

    let confirmed = dir.path().join("old.flac");
    std::fs::write(&confirmed, b"there first").unwrap();
    record_user_choice(&confirmed, true).unwrap();
    assert!(authorisation_for(&confirmed).unwrap().replace_confirmed());
}

/// Appending left the oldest entry in front of the lookup for good. A retry kept a stale
/// `replace_confirmed` and failed with `AlreadyExists` however often the user re-confirmed.
#[test]
fn a_fresh_answer_supersedes_an_unspent_one_for_the_same_path() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"there first").unwrap();

    record_user_choice(&dest, false).unwrap();
    record_user_choice(&dest, true).unwrap();

    assert!(
        authorisation_for(&dest)
            .expect("authorised")
            .replace_confirmed(),
        "the lookup must see the newest answer, not the first"
    );

    // And only one entry exists; consuming leaves nothing redeemable behind.
    consume(authorisation_for(&dest).expect("authorised"));
    assert!(authorisation_for(&dest).is_none());
}

/// Canonicalising the whole path made a link and its target one key. "replace link.flac?" produced
/// a confirmed entry for a file the user was never shown.
#[cfg(unix)]
#[test]
fn a_symlinks_authorisation_does_not_reach_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.flac");
    std::fs::write(&target, b"target").unwrap();
    let link = dir.path().join("link.flac");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    record_user_choice(&link, true).unwrap();

    assert!(
        authorisation_for(&link).is_some(),
        "the answered path is authorised"
    );
    assert!(
        authorisation_for(&target).is_none(),
        "the file behind the link was never offered to the user"
    );
}

/// Spending by path removed the fresh entry: consent the user had just given was consumed by a
/// write that never used it.
#[test]
fn a_newer_answer_for_the_same_path_is_not_spent_by_an_older_download() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");

    record_user_choice(&dest, false).unwrap();
    let in_flight = authorisation_for(&dest).expect("matched before the write");
    record_user_choice(&dest, true).unwrap();

    consume(in_flight);
    assert!(
        authorisation_for(&dest)
            .expect("the fresh answer must survive")
            .replace_confirmed(),
        "spending the older authorisation must not touch the newer entry"
    );
}

/// A relative path resolves against whatever the working directory is when the write runs, not
/// against what the user answered: spelled relative, even an authorised file must not match.
/// `./name` is the spelling that resolves; a bare `name` has no parent to canonicalise and would
/// be refused with or without the check.
#[test]
fn a_path_that_is_not_absolute_is_refused() {
    let name = "a-relative-spelling-of-an-answer.flac";
    let chosen = std::env::current_dir().unwrap().join(name);
    record_user_choice(&chosen, false).unwrap();

    assert!(authorisation_for(&std::path::Path::new(".").join(name)).is_none());
    assert!(
        authorisation_for(&chosen).is_some(),
        "the absolute spelling is the one answered"
    );
}

/// A destination already held takes the fresh answer in its own entry and needs no slot; a new one
/// at capacity costs the oldest. Bounding before deduplicating dropped an unrelated authorisation,
/// and the download holding it was then refused for a dialog its user had answered.
#[test]
fn a_held_destination_needs_no_slot_and_a_new_one_at_capacity_costs_the_oldest() {
    for (what, destination, replace_confirmed, oldest_survives) in [
        ("re-answering a held path", "/music/7.flac", true, true),
        ("a new destination", "/music/fresh.flac", false, false),
    ] {
        let mut choices = Vec::new();
        for n in 0..MAX_USER_CHOICES {
            choices.push(Choice {
                id: n as u64,
                resolved: PathBuf::from(format!("/music/{n}.flac")),
                replace_confirmed: false,
            });
        }

        record_bounded(
            &mut choices,
            Choice {
                id: 900,
                resolved: PathBuf::from(destination),
                replace_confirmed,
            },
        );

        assert_eq!(choices.len(), MAX_USER_CHOICES, "{what}");
        assert_eq!(
            choices
                .iter()
                .any(|c| c.resolved.as_path() == Path::new("/music/0.flac")),
            oldest_survives,
            "{what}: the oldest goes only when the new entry really is new"
        );
        let held = choices
            .iter()
            .filter(|c| c.resolved.as_path() == Path::new(destination))
            .collect::<Vec<_>>();
        assert_eq!(held.len(), 1, "{what}: one entry per destination");
        assert_eq!(
            held[0].replace_confirmed, replace_confirmed,
            "{what}: the newest answer wins"
        );
    }
}

/// Nobody was asked about any particular file inside a granted folder; nothing there may be
/// destroyed. The single-track path keeps its own answer and is unaffected.
#[test]
fn a_folder_grant_never_confirms_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path()).unwrap();

    let auth = authorisation_for(&dir.path().join("song.flac")).expect("covered by the folder");
    assert!(!auth.replace_confirmed());
    assert!(auth.skips_existing());
}

/// A file answer is the more specific consent and the only one carrying a replace decision: it
/// must win where both cover the same path.
#[test]
fn a_file_answer_wins_over_a_folder_grant_for_the_same_path() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"there first").unwrap();
    record_folder_grant(dir.path()).unwrap();
    record_user_choice(&dest, true).unwrap();

    let auth = authorisation_for(&dest).expect("authorised");
    assert!(auth.replace_confirmed(), "the save dialog asked about it");
    assert!(!auth.skips_existing());
}

/// The folder grant lasts the session: an album is many writes behind one answer; spending it on
/// the first track would refuse every track after it.
#[test]
fn consuming_a_folder_grant_leaves_it_usable() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path()).unwrap();

    let first = authorisation_for(&dir.path().join("one.flac")).expect("covered");
    consume(first);

    assert!(
        authorisation_for(&dir.path().join("two.flac")).is_some(),
        "the rest of the album still has to be writable"
    );
}

/// `..` is refused before either ledger is consulted. A folder grant cannot be reached by a path
/// that climbs out of it and back in. Guards the new branch against the traversal the file branch
/// already refuses.
#[test]
fn a_traversal_cannot_reach_a_granted_folder() {
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("music");
    std::fs::create_dir(&granted).unwrap();
    record_folder_grant(&granted).unwrap();

    assert!(authorisation_for(&granted.join("..").join("music").join("song.flac")).is_none());
    assert!(
        authorisation_for(&granted.join("song.flac")).is_some(),
        "the direct path still works"
    );
}

/// A prefix match on strings would accept `/tmp/musicX` for a grant on `/tmp/music`.
#[test]
fn a_sibling_directory_sharing_a_prefix_is_not_covered() {
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("music");
    let sibling = root.path().join("musicX");
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    record_folder_grant(&granted).unwrap();

    let dest = resolved(&sibling.join("song.flac")).expect("resolvable");
    assert!(!folder_grant_covers(&dest));
}

/// A directory that does not exist cannot be canonicalised: it could never become a key the write
/// side agrees with. Refused at record time rather than stored unresolved.
#[test]
fn a_folder_that_does_not_exist_is_not_granted() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");
    let _ = record_folder_grant(&absent);

    let dest = resolved(&absent.join("song.flac")).expect("resolvable");
    assert!(!folder_grant_covers(&dest));
}

/// Bounded for the same reason as the file ledger. Exercised on a local vector, never the global:
/// libtest runs tests as threads in one process; evicting from the real ledger here would drop
/// grants a concurrent test is holding.
#[test]
fn the_oldest_grant_is_dropped_past_the_bound() {
    let mut granted = Vec::new();
    for n in 0..MAX_GRANTED_FOLDERS {
        push_bounded(&mut granted, PathBuf::from(format!("/music/{n}")));
    }
    push_bounded(&mut granted, PathBuf::from("/music/one-too-many"));

    assert_eq!(granted.len(), MAX_GRANTED_FOLDERS);
    assert!(
        !granted.contains(&PathBuf::from("/music/0")),
        "the oldest must be the one dropped"
    );
    assert!(granted.contains(&PathBuf::from("/music/one-too-many")));
}

/// Re-picking the same folder is the same consent, not a second one: appending would let a user
/// evict their own earlier grants by answering one dialog repeatedly.
#[test]
fn re_granting_the_same_folder_does_not_add_a_second_entry() {
    let mut granted = Vec::new();
    push_bounded(&mut granted, PathBuf::from("/music"));
    push_bounded(&mut granted, PathBuf::from("/music"));

    assert_eq!(granted, vec![PathBuf::from("/music")]);
}

/// The bypass this closes: popping `..` lexically matched an authorisation beside the link, while the
/// write landed beside the link's target.
#[cfg(unix)]
#[test]
fn a_traversal_after_a_symlink_does_not_match_the_link_side() {
    let root = tempfile::tempdir().unwrap();
    let safe = root.path().join("safe");
    let other = root.path().join("other");
    std::fs::create_dir(&safe).unwrap();
    std::fs::create_dir_all(other.join("child")).unwrap();
    std::os::unix::fs::symlink(other.join("child"), safe.join("link")).unwrap();

    record_user_choice(&safe.join("song.flac"), false).unwrap();

    assert!(
        authorisation_for(&safe.join("link").join("..").join("song.flac")).is_none(),
        "the write would land beside the link's target, not beside the link"
    );
}

/// An install folder holding `frontend/`, beside the data dir as on Windows, and the guard built
/// over it.
fn guarded_install() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    crate::platform::install_guard::Protected,
) {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("install");
    std::fs::create_dir_all(root.join("frontend")).unwrap();
    let guard =
        crate::platform::install_guard::Protected::new(&root, &parent.path().join("data/native"));
    (parent, root, guard)
}

/// A page builds the blob: its bytes are a plugin's as much as TIDAL's, and never land in the
/// app's own files, whatever the user browsed to. Windows loads a DLL from the exe's folder before
/// System32: a blob saved there runs with the app.
#[test]
fn a_destination_inside_the_install_folder_is_refused() {
    let (_parent, root, guard) = guarded_install();
    for inside in ["frontend/native-host.cjs", "version.dll"] {
        assert_eq!(
            vet_destination(Some(&guard), &root.join(inside)),
            Err(NotRecorded::Protected),
            "{inside}"
        );
    }
}

/// `dir` reached through a symlink on unix. A temp dir there is already canonical; without the
/// link, a verdict that hands back the path as written would pass for the resolved one.
fn through_a_link(dir: &std::path::Path) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let link = dir.with_extension("link");
        std::os::unix::fs::symlink(dir, &link).unwrap();
        link
    }
    #[cfg(not(unix))]
    {
        dir.to_path_buf()
    }
}

#[test]
fn an_ordinary_destination_is_kept_as_the_write_resolves_it() {
    let (parent, _root, guard) = guarded_install();
    let exports = parent.path().join("exports");
    std::fs::create_dir(&exports).unwrap();
    let answer = through_a_link(&exports).join("export.json");
    assert_eq!(
        vet_destination(Some(&guard), &answer),
        Ok(exports.canonicalize().unwrap().join("export.json"))
    );
}

#[test]
fn the_install_folder_is_refused_as_a_folder() {
    let (_parent, root, guard) = guarded_install();
    assert_eq!(vet_folder(Some(&guard), &root), Err(NotRecorded::Protected));
}
