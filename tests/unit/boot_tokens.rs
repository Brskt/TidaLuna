//! Tests for `src/boot_tokens.rs`, attached to it by `#[path]`.

use super::*;
use crate::platform;
use platform::secure_store::TokenGeneration;

/// A nonce of the shape the generator makes, told apart by `role` and `tag`.
fn nonce(role: char, tag: &str) -> String {
    let hex: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    format!("luna_{:0<32}", format!("{role}{hex}"))
}

fn generation(tag: &str) -> TokenGeneration {
    TokenGeneration {
        access_token: format!("real_at_{tag}"),
        refresh_token: format!("real_rt_{tag}"),
        opaque_at: nonce('a', tag),
        opaque_rt: nonce('b', tag),
        version: 1,
        access_expires: u64::MAX,
        user_id: Some("42".into()),
        granted_scopes: vec!["r_usr".into()],
        client: platform::secure_store::ClientBinding::Proven("cid".into()),
    }
}

fn stored(current: TokenGeneration, previous: Option<TokenGeneration>) -> StoredTokenState {
    StoredTokenState {
        current,
        previous,
        previous_valid_until: None,
    }
}

const UUID: &str = "0f1e2d3c-4b5a-4978-8796-a5b4c3d2e1f0";

/// One blob as the SDK writes it: the credential JSON under AES-CTR, its key wrapped with AES-KW
/// under PBKDF2 of `password`, which the SDK takes to be the id itself.
struct Sealed<'a> {
    id: &'a str,
    password: &'a str,
    access_token: &'a str,
    refresh_token: Option<&'a str>,
    user: Option<&'a str>,
}

impl Sealed<'_> {
    fn entries(&self) -> Vec<(String, Vec<u8>)> {
        use aes::cipher::{KeyInit, KeyIvInit, StreamCipher};

        let (salt, counter, data_key) = ([7u8; 16], [9u8; 16], [11u8; 32]);
        let mut access = serde_json::json!({ "token": self.access_token });
        if let Some(user) = self.user {
            access["userId"] = user.into();
        }
        let mut credentials = serde_json::json!({ "accessToken": access });
        if let Some(rt) = self.refresh_token {
            credentials["refreshToken"] = rt.into();
        }
        let mut data = serde_json::to_vec(&credentials).unwrap();
        ctr::Ctr64BE::<aes::Aes256>::new_from_slices(&data_key, &counter)
            .unwrap()
            .apply_keystream(&mut data);
        let mut wrapping_key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
            self.password.as_bytes(),
            &salt,
            100_000,
            &mut wrapping_key,
        );
        let mut wrapped = [0u8; 40];
        aes_kw::KwAes256::new_from_slice(&wrapping_key)
            .unwrap()
            .wrap_key(&data_key, &mut wrapped)
            .unwrap();
        [
            ("Salt", salt.to_vec()),
            ("Counter", counter.to_vec()),
            ("Key", wrapped.to_vec()),
            ("Data", data),
        ]
        .into_iter()
        .map(|(suffix, value)| (format!("AuthDB/{}{suffix}", self.id), value))
        .collect()
    }
}

fn user_blob<'a>(id: &'a str, at: &'a str, rt: &'a str) -> Sealed<'a> {
    Sealed {
        id,
        password: id,
        access_token: at,
        refresh_token: Some(rt),
        user: Some("42"),
    }
}

/// The SDK's app-level blob: under its own id, an access token and no user.
fn app_blob<'a>(id: &'a str, at: &'a str) -> Sealed<'a> {
    Sealed {
        id,
        password: id,
        access_token: at,
        refresh_token: None,
        user: None,
    }
}

/// A Local Storage holding exactly `blobs`, plus one key of the origin no purge may touch.
fn local_storage(blobs: &[Sealed]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut entries: Vec<(String, Vec<u8>)> = blobs.iter().flat_map(Sealed::entries).collect();
    entries.push(("AuthDBx".into(), b"neighbour".to_vec()));
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_slice()))
        .collect();
    sdk_storage::write_ls_values(&dir.path().join("leveldb"), &refs).unwrap();
    dir
}

/// The ids still in Local Storage, and whether the neighbour key survived.
fn survivors(dir: &tempfile::TempDir) -> (Vec<String>, bool) {
    let leveldb = dir.path().join("leveldb");
    let neighbour = matches!(
        sdk_storage::read_ls_value(&leveldb, "AuthDBx"),
        sdk_storage::LsValue::Value(v) if v == b"neighbour"
    );
    let OpenedCredentials::Opened(store) = sdk_storage::open_credentials(&leveldb) else {
        panic!("Local Storage did not reopen");
    };
    let ids = store.blobs().iter().map(|blob| blob.id.clone()).collect();
    (ids, neighbour)
}

/// The boot restores on the page's own blob and purges every blob it does not keep before CEF
/// starts, real tokens included even when they are the store's: their password sits beside them.
/// A store with no blob of its own restores nothing and every credential goes.
#[test]
fn a_boot_restores_on_the_pages_blob_and_purges_every_other() {
    let (cur, new, old) = (generation("cur"), generation("new"), generation("old"));
    let app = format!("{UUID}_clientCredentials_PROD");
    let app_nonce = nonce('c', "app");
    /// What a boot starts from, what it restores, and the ids it leaves.
    type Row<'a> = (
        &'a str,
        Vec<Sealed<'a>>,
        StoredTokenState,
        Option<BlobNames>,
        Vec<&'a str>,
    );
    let rows: Vec<Row> = vec![
        (
            "current nonces",
            vec![user_blob(UUID, &cur.opaque_at, &cur.opaque_rt)],
            stored(cur.clone(), None),
            Some(BlobNames::Mine),
            vec![UUID],
        ),
        (
            "current real tokens",
            vec![user_blob(UUID, &cur.access_token, &cur.refresh_token)],
            stored(cur.clone(), None),
            Some(BlobNames::RealTokens),
            vec![],
        ),
        (
            "previous real tokens",
            vec![user_blob(UUID, &old.access_token, &old.refresh_token)],
            stored(new.clone(), Some(old.clone())),
            Some(BlobNames::RealTokens),
            vec![],
        ),
        (
            "the page's nonces beside its app blob and a real seed under another id",
            vec![
                user_blob(UUID, &cur.opaque_at, &cur.opaque_rt),
                app_blob(&app, &app_nonce),
                user_blob("tidal", &cur.access_token, &cur.refresh_token),
            ],
            stored(cur.clone(), None),
            Some(BlobNames::Mine),
            vec![UUID, app.as_str()],
        ),
        (
            "real tokens the store does not hold",
            vec![user_blob(UUID, "stranger_at", "stranger_rt")],
            stored(cur.clone(), Some(old.clone())),
            None,
            vec![],
        ),
        (
            "the page's nonces sealed under another password",
            vec![Sealed {
                password: "tidal",
                ..user_blob(UUID, &cur.opaque_at, &cur.opaque_rt)
            }],
            stored(cur.clone(), None),
            None,
            vec![],
        ),
        (
            "no blob at all",
            vec![],
            stored(cur.clone(), None),
            None,
            vec![],
        ),
    ];
    for (what, blobs, store, expected, kept) in rows {
        let dir = local_storage(&blobs);
        let restored = match reconcile(Ok(Some(store)), &dir.path().join("leveldb")) {
            BootRestore::Restored { names, .. } => Some(names),
            BootRestore::BlobUnusable => None,
            _ => panic!("{what}: neither restored nor unusable"),
        };
        assert_eq!(restored, expected, "{what}");
        let mut kept: Vec<String> = kept.into_iter().map(str::to_owned).collect();
        kept.sort();
        assert_eq!(survivors(&dir), (kept, true), "{what}");
    }
}

/// Without a store the app can read, the page's blobs are no session Rust can serve: every one
/// goes before the SDK reads it, whatever it holds.
#[test]
fn a_boot_without_a_readable_store_purges_every_credential() {
    let cur = generation("cur");
    let app = format!("{UUID}_clientCredentials_PROD");
    let app_nonce = nonce('c', "app");
    for (what, loaded) in [
        ("no store", Ok(None)),
        ("a corrupt store", Err(StoreError::Corrupt)),
        (
            "an unavailable store",
            Err(StoreError::Backend("keychain locked".into())),
        ),
    ] {
        let dir = local_storage(&[
            user_blob(UUID, &cur.opaque_at, &cur.opaque_rt),
            app_blob(&app, &app_nonce),
        ]);
        let restore = reconcile(loaded, &dir.path().join("leveldb"));
        assert_eq!(restore.page_blob(), PageBlob::PurgeAll, "{what}");
        assert_eq!(survivors(&dir), (vec![], true), "{what}");
    }
}

fn credentials(at: &str, rt: Option<&str>, user: Option<&str>) -> sdk_storage::SdkCredentials {
    sdk_storage::SdkCredentials {
        access_token: Some(sdk_storage::SdkAccessToken {
            token: Some(at.to_owned()),
            user_id: user.map(str::to_owned),
        }),
        refresh_token: rt.map(str::to_owned),
    }
}

/// A blob of nonces written for the store's user is the page's own, whatever generation its nonces
/// name: a strict match left a store from any session without a page refresh on the sign-in
/// screen. Another user's, a half pair or an unknown real token restores nothing.
#[test]
fn a_nonce_blob_of_the_stores_user_is_the_pages_whatever_its_generation() {
    let (new, old) = (generation("new"), generation("old"));
    let store = stored(new.clone(), Some(old.clone()));
    let mut anonymous = new.clone();
    anonymous.user_id = None;
    let without_user = stored(anonymous, None);
    let unknown = nonce('a', "gone");
    let unknown_rt = nonce('b', "gone");
    for (blob, store, expected, why) in [
        (
            credentials(&unknown, Some(&unknown_rt), Some("42")),
            &store,
            BlobNames::Mine,
            "nonces of the store's user",
        ),
        (
            credentials(&unknown, Some(&unknown_rt), Some("43")),
            &store,
            BlobNames::Foreign,
            "another user",
        ),
        (
            credentials(&unknown, Some(&unknown_rt), None),
            &store,
            BlobNames::Foreign,
            "a blob naming no user",
        ),
        (
            credentials(&new.opaque_at, Some(&new.opaque_rt), Some("42")),
            &without_user,
            BlobNames::Foreign,
            "a store naming no user",
        ),
        (
            credentials(&new.opaque_at, Some("real_rt_new"), Some("42")),
            &store,
            BlobNames::Foreign,
            "a nonce and a real token",
        ),
        (
            credentials(&unknown, None, None),
            &store,
            BlobNames::App,
            "an application credential",
        ),
        (
            credentials("luna_at_new", Some("luna_rt_new"), Some("42")),
            &store,
            BlobNames::Foreign,
            "values short of a nonce's shape",
        ),
    ] {
        assert_eq!(BlobNames::classify(&blob, store), expected, "{why}");
    }
}

/// Real tokens the page may have read are rotated away by the boot refresh, never kept valid.
#[test]
fn a_blob_of_real_tokens_has_them_rotated_at_boot() {
    assert_eq!(
        BlobNames::RealTokens.duty(),
        app_state::RefreshDuty::CheckAndRotate
    );
}
