//! Tests for `src/platform/secure_store.rs`, attached to it by `#[path]`.
//!
//! The storage halves reach a real Keychain, DPAPI or a file: they are not
//! exercised here. What is testable, and what a defect turned on, is the rule
//! deciding whether a generation is worth storing at all.

use super::*;

/// A generation shaped like the ones both token paths mint. The refresh pair is
/// the caller's to set, being the subject here.
fn generation(refresh_token: &str, opaque_rt: &str) -> TokenGeneration {
    TokenGeneration {
        access_token: "real-access-token".to_owned(),
        refresh_token: refresh_token.to_owned(),
        opaque_at: "luna_aaaa".to_owned(),
        opaque_rt: opaque_rt.to_owned(),
        version: 1,
        access_expires: 0,
        user_id: None,
        granted_scopes: Vec::new(),
        client_id: "client".to_owned(),
    }
}

#[test]
fn a_generation_carrying_a_refresh_token_is_durable() {
    assert!(generation("real-refresh-token", "luna_bbbb").is_durable());
}

#[test]
fn a_generation_without_a_refresh_token_is_not_durable() {
    // Exactly what both token paths mint when a response carries no
    // refresh_token and no prior generation is in memory to reuse one from: the
    // `(String::new(), String::new())` arm. It serves the running session from
    // memory and is worth nothing on disk, because nothing in it outlives the
    // access token's expiry.
    assert!(!generation("", "").is_durable());
}

#[test]
fn a_live_token_still_reads_as_live_on_the_millisecond_clock_the_sdk_uses() {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before the epoch")
        .as_secs();
    let mut cur_gen = generation("real-refresh-token", "luna_bbbb");
    cur_gen.access_expires = now_secs + 3600;

    let now_ms = now_secs * 1000;
    assert!(cur_gen.access_expires_ms() > now_ms);

    // The control that keeps the assertion above from being vacuous, and the
    // defect it pins: the raw field is what used to be seeded, and it fails the
    // very same comparison by three orders of magnitude - an hour of life left
    // read as decades expired.
    assert!(cur_gen.access_expires < now_ms);
}

#[test]
fn an_opaque_refresh_nonce_alone_does_not_make_a_generation_durable() {
    // The renderer only ever sees the opaque nonce; a generation could carry
    // one while the real token behind it is gone. Storing that would hand the
    // next launch a credential it cannot redeem.
    assert!(!generation("", "luna_bbbb").is_durable());
}

#[cfg(target_os = "linux")]
mod file_durability {
    use super::*;
    use crate::platform::syscall_trace::{Event, traced};

    const FILE: &str = "auth_tokens.json";

    fn stored_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        save_platform(dir.path(), b"{\"stored\":true}").unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        (dir, canonical)
    }

    #[test]
    fn erasing_the_token_file_empties_it_then_unlinks_it_then_flushes_the_directory() {
        let (dir, canonical) = stored_dir();

        let (erased, events) = traced(|| delete_platform(dir.path()));

        erased.unwrap();
        assert!(!canonical.join(FILE).exists());
        assert_eq!(
            events,
            vec![
                Event::Fsync {
                    path: canonical.join(FILE),
                    directory: false
                },
                Event::Unlink(dir.path().join(FILE)),
                Event::Fsync {
                    path: canonical.clone(),
                    directory: true
                },
            ]
        );
    }

    #[test]
    fn saving_the_token_file_flushes_the_directory_after_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();

        let (saved, events) = traced(|| save_platform(dir.path(), b"{\"stored\":true}"));

        saved.unwrap();
        // The staged file's name is random: its entry is told apart by not being a directory.
        let [
            Event::Fsync {
                directory: false, ..
            },
            flushed,
        ] = events.as_slice()
        else {
            panic!("the file, then its directory: {events:?}");
        };
        assert_eq!(
            flushed,
            &Event::Fsync {
                path: canonical,
                directory: true
            }
        );
    }

    #[test]
    fn a_token_file_left_empty_by_an_erase_whose_unlink_was_lost_loads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), b"").unwrap();

        assert!(matches!(load(dir.path()), Ok(None)));
    }

    #[test]
    fn a_token_file_with_a_stored_state_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let state = StoredTokenState {
            current: generation("real-refresh-token", "luna_bbbb"),
            previous: None,
            previous_valid_until: None,
        };
        save_platform(dir.path(), &serde_json::to_vec(&state).unwrap()).unwrap();

        let loaded = load(dir.path()).unwrap().expect("a stored state");
        assert_eq!(loaded.current.refresh_token, "real-refresh-token");
    }
}
