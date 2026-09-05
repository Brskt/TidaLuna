//! Tests for `src/ui/file_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// Fill CEF's dispatch table before the first `cef_*` call in this file.
///
/// macOS resolves the framework at launch rather than linking it; every `cef_*` entry point
/// is a trampoline through a pointer that `cef_load_library` writes. The production process
/// does that in `main`; a test binary has no `main`, so calling one first jumps to address
/// zero, surfacing as `KERN_INVALID_ADDRESS at 0x0`.
#[cfg(target_os = "macos")]
fn ensure_cef_loaded() {
    crate::platform::cef_loader::ensure_framework_loaded();
}

/// Windows points the OS loader at `bin/cef` and Linux leans on an rpath: the table is
/// filled before the first instruction of the process either way.
#[cfg(not(target_os = "macos"))]
fn ensure_cef_loaded() {}

/// The regression this guards is a dismissed dialog reporting no selection at all, which
/// `showSaveDialog` then reported to the plugin as `canceled`.
#[test]
fn a_borrowed_list_is_read_through_the_raw_pointer() {
    ensure_cef_loaded();
    let mut owned = CefStringList::new();
    owned.append("/music/song.flac");

    // The same shape CEF hands the callback: a borrow of a list it still owns.
    let raw: *mut sys::_cef_string_list_t = (&mut owned).into();
    let mut borrowed = CefStringList::from(raw);

    assert_eq!(
        read_paths(&mut borrowed),
        vec!["/music/song.flac".to_string()]
    );
}

/// A plugin names its own destination: the only thing that makes one legitimate is that the
/// user answered a dialog for it. A path the renderer invented matches nothing.
#[test]
fn a_destination_no_dialog_produced_is_not_authorised() {
    let dir = tempfile::tempdir().unwrap();
    assert!(authorisation_for(&dir.path().join("song.flac")).is_none());
}

/// Matching does not consume: a fetch that fails after the check must not cost the user their
/// answer.
#[test]
fn an_authorisation_survives_until_it_is_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let chosen = dir.path().join("song.flac");
    record_user_choice(&chosen, false);

    assert!(authorisation_for(&chosen).is_some(), "checked once");
    let auth = authorisation_for(&chosen).expect("a failed attempt must not burn it");

    consume(auth);
    assert!(
        authorisation_for(&chosen).is_none(),
        "one dialog answer buys one written file"
    );
}

/// The destination need not exist yet: it may name a tree `create_dir_all` will build, so
/// canonicalising the parent outright would refuse exactly that case.
#[test]
fn a_destination_below_a_directory_that_does_not_exist_yet_is_authorised() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("Artist").join("Album").join("song.flac");
    record_user_choice(&deep, false);

    assert!(authorisation_for(&deep).is_some());
}

/// String comparison would accept this: it carries the authorised directory as a prefix and ends with
/// the authorised name.
#[test]
fn a_traversal_that_leaves_the_authorised_file_is_not_authorised() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let chosen = inner.join("song.flac");
    record_user_choice(&chosen, false);

    assert!(authorisation_for(&inner.join("..").join("song.flac")).is_none());
    assert!(
        authorisation_for(&chosen).is_some(),
        "the real one survived the attempt"
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
    record_user_choice(&chosen, false);

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
    record_user_choice(&unconfirmed, false);
    assert!(!authorisation_for(&unconfirmed).unwrap().replace_confirmed());

    let confirmed = dir.path().join("old.flac");
    std::fs::write(&confirmed, b"there first").unwrap();
    record_user_choice(&confirmed, true);
    assert!(authorisation_for(&confirmed).unwrap().replace_confirmed());
}

/// Appending left the oldest entry in front of the lookup for good. A retry kept a stale
/// `replace_confirmed` and failed with `AlreadyExists` however often the user re-confirmed.
#[test]
fn a_fresh_answer_supersedes_an_unspent_one_for_the_same_path() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    std::fs::write(&dest, b"there first").unwrap();

    record_user_choice(&dest, false);
    record_user_choice(&dest, true);

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

    record_user_choice(&link, true);

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

    record_user_choice(&dest, false);
    let in_flight = authorisation_for(&dest).expect("matched before the write");
    record_user_choice(&dest, true);

    consume(in_flight);
    assert!(
        authorisation_for(&dest)
            .expect("the fresh answer must survive")
            .replace_confirmed(),
        "spending the older authorisation must not touch the newer entry"
    );
}

/// One dialog answer buys one written file; the entry has to be gone afterwards.
#[test]
fn consuming_uses_the_identity_matched_before_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    record_user_choice(&dest, false);

    let auth = authorisation_for(&dest).expect("authorised");
    consume(auth);

    assert!(
        authorisation_for(&dest).is_none(),
        "the entry must be gone, not left redeemable"
    );
}

/// Refused whether or not a real directory sits behind it. Tracking each kernel's own `..` semantics
/// separately is what produced the traversal bypass.
#[test]
fn a_traversal_is_refused_with_or_without_a_directory_behind_it() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("song.flac");
    record_user_choice(&dest, false);

    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(
        authorisation_for(&sub.join("..").join("song.flac")).is_none(),
        "real directory"
    );
    assert!(
        authorisation_for(&dir.path().join("absent").join("..").join("song.flac")).is_none(),
        "no directory behind it"
    );
}

#[test]
fn a_path_that_is_not_absolute_is_refused() {
    assert!(authorisation_for(std::path::Path::new("relative.flac")).is_none());
    assert!(authorisation_for(std::path::Path::new("/..")).is_none());
}

/// Re-answering a dialog for a path already held replaces that entry. It needs no slot. Bounding
/// before deduplicating dropped an unrelated authorisation anyway, and the download holding it was
/// then refused for a dialog its user had answered.
#[test]
fn re_answering_a_held_path_at_capacity_evicts_nothing() {
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
            resolved: PathBuf::from("/music/7.flac"),
            replace_confirmed: true,
        },
    );

    assert_eq!(choices.len(), MAX_USER_CHOICES);
    assert!(
        choices
            .iter()
            .any(|c| c.resolved.as_path() == Path::new("/music/0.flac")),
        "the oldest unrelated answer must survive a replacement"
    );
    let replaced = choices
        .iter()
        .filter(|c| c.resolved.as_path() == Path::new("/music/7.flac"))
        .collect::<Vec<_>>();
    assert_eq!(replaced.len(), 1, "one entry per destination");
    assert!(replaced[0].replace_confirmed, "the newest answer wins");
}

/// A genuinely new destination at capacity still costs the oldest entry (that bound is the point).
#[test]
fn a_new_destination_at_capacity_drops_the_oldest() {
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
            resolved: PathBuf::from("/music/fresh.flac"),
            replace_confirmed: false,
        },
    );

    assert_eq!(choices.len(), MAX_USER_CHOICES);
    assert!(
        !choices
            .iter()
            .any(|c| c.resolved.as_path() == Path::new("/music/0.flac")),
        "the oldest goes when the new entry really is new"
    );
}

/// Nobody was asked about any particular file inside a granted folder; nothing there may be
/// destroyed. The single-track path keeps its own answer and is unaffected.
#[test]
fn a_folder_grant_never_confirms_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path());

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
    record_folder_grant(dir.path());
    record_user_choice(&dest, true);

    let auth = authorisation_for(&dest).expect("authorised");
    assert!(auth.replace_confirmed(), "the save dialog asked about it");
    assert!(!auth.skips_existing());
}

/// The folder grant lasts the session: an album is many writes behind one answer; spending it on
/// the first track would refuse every track after it.
#[test]
fn consuming_a_folder_grant_leaves_it_usable() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path());

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
    record_folder_grant(&granted);

    assert!(authorisation_for(&granted.join("..").join("music").join("song.flac")).is_none());
    assert!(
        authorisation_for(&granted.join("song.flac")).is_some(),
        "the direct path still works"
    );
}

/// An album writes many files after one folder answer. The grant covers the directory rather
/// than a name inside it.
#[test]
fn a_granted_folder_covers_a_file_directly_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path());

    let dest = resolved(&dir.path().join("song.flac")).expect("resolvable");
    assert!(folder_grant_covers(&dest));
}

/// `pathFormat` is a user-editable template in the plugin; a `/` in it writes into subdirectories
/// the user never named. Refusing those would break exactly the users who organise their library.
#[test]
fn a_granted_folder_covers_a_subdirectory_below_it() {
    let dir = tempfile::tempdir().unwrap();
    record_folder_grant(dir.path());

    let dest =
        resolved(&dir.path().join("Artist").join("Album").join("song.flac")).expect("resolvable");
    assert!(folder_grant_covers(&dest));
}

/// A prefix match on strings would accept `/tmp/musicX` for a grant on `/tmp/music`.
#[test]
fn a_sibling_directory_sharing_a_prefix_is_not_covered() {
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("music");
    let sibling = root.path().join("musicX");
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    record_folder_grant(&granted);

    let dest = resolved(&sibling.join("song.flac")).expect("resolvable");
    assert!(!folder_grant_covers(&dest));
}

/// Nothing granted means nothing covered. Guards against an empty ledger matching by accident.
#[test]
fn no_grant_covers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let dest = resolved(&dir.path().join("song.flac")).expect("resolvable");
    assert!(!folder_grant_covers(&dest));
}

/// A directory that does not exist cannot be canonicalised: it could never become a key the write
/// side agrees with. Refused at record time rather than stored unresolved.
#[test]
fn a_folder_that_does_not_exist_is_not_granted() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");
    record_folder_grant(&absent);

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

    record_user_choice(&safe.join("song.flac"), false);

    assert!(
        authorisation_for(&safe.join("link").join("..").join("song.flac")).is_none(),
        "the write would land beside the link's target, not beside the link"
    );
}
