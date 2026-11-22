use crate::platform::sdk_storage::{self, OpenedCredentials, Purge};
use crate::platform::secure_store::{self, StoreError, StoredTokenState};
use crate::{app_state, verr, vprintln};
use std::path::Path;

/// What a stored SDK credential blob is, against the secure store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobNames {
    /// Both tokens are nonces and the blob was written for the store's user: the page's own
    /// session, whichever generation its nonces name. A nonce of an older generation resolves to
    /// the current one on the wire all the same.
    Mine,
    /// An application credential: an access nonce, no refresh token, no user.
    App,
    /// Real tokens, equal to a generation of the store.
    RealTokens,
    /// Undecryptable, a half pair, another or an unknown user, a real token the store does not
    /// hold: nothing to restore the page on.
    Foreign,
}

impl BlobNames {
    fn classify(credentials: &sdk_storage::SdkCredentials, store: &StoredTokenState) -> Self {
        let access = credentials.access_token.as_ref();
        let at = access.and_then(|a| a.token.as_deref()).unwrap_or("");
        let rt = credentials.refresh_token.as_deref().unwrap_or("");
        let blob_user = access
            .and_then(|a| a.user_id.as_deref())
            .filter(|user| !user.is_empty());
        let store_user = store
            .current
            .user_id
            .as_deref()
            .or(store.previous.as_ref().and_then(|p| p.user_id.as_deref()));
        let is_nonce = crate::ui::token_filter::is_nonce;
        if is_nonce(at) && is_nonce(rt) {
            return if blob_user.is_some() && blob_user == store_user {
                Self::Mine
            } else {
                Self::Foreign
            };
        }
        if is_nonce(at) && rt.is_empty() && blob_user.is_none() {
            return Self::App;
        }
        let held = std::iter::once(&store.current).chain(store.previous.as_ref());
        if !rt.is_empty()
            && held
                .into_iter()
                .any(|g| g.access_token == at && g.refresh_token == rt)
        {
            return Self::RealTokens;
        }
        Self::Foreign
    }

    /// A blob of Local Storage, decrypted with its own id: one that does not decrypt holds nothing
    /// to restore.
    fn of_blob(blob: &sdk_storage::CredentialBlob, store: &StoredTokenState) -> Self {
        blob.decrypt().map_or(Self::Foreign, |credentials| {
            Self::classify(&credentials, store)
        })
    }

    /// Whether the page keeps the blob. Real tokens go even when they are the store's: their
    /// password is the id beside them, readable by any plugin.
    fn stays(self) -> bool {
        matches!(self, Self::Mine | Self::App)
    }

    /// The boot refresh is owed whatever the page holds: a nonce blob alone never proves its
    /// refresh token alive.
    fn duty(self) -> app_state::RefreshDuty {
        match self {
            Self::Mine | Self::App => app_state::RefreshDuty::Check,
            Self::RealTokens => app_state::RefreshDuty::CheckAndRotate,
            Self::Foreign => app_state::RefreshDuty::Spent,
        }
    }
}

/// What the boot made of the secure store and the SDK's blobs.
enum BootRestore {
    /// A blob is the page's session for the store: restored, and the blobs `purge` names go.
    Restored {
        tokens: Box<StoredTokenState>,
        names: BlobNames,
        purge: Vec<String>,
    },
    /// No secure store: nobody is signed in.
    StoreAbsent,
    /// The secure store does not decode.
    StoreCorrupt,
    /// The keychain or file backend failed; the store itself may read at the next launch.
    StoreUnavailable,
    /// The store loaded, and no blob is the page's session for it.
    BlobUnusable,
    /// Local Storage would not open or read.
    ReconcileFailed,
}

/// What becomes of the SDK's blobs before CEF starts.
#[derive(Debug, PartialEq, Eq)]
enum PageBlob<'a> {
    Keep,
    /// These ids go; the page's session stays.
    Purge(&'a [String]),
    /// Every credential of the origin goes: Rust restored no session the page could use.
    PurgeAll,
}

impl BootRestore {
    /// Runs the 100k-iteration PBKDF2 once per blob.
    fn decide(
        loaded: Result<Option<StoredTokenState>, StoreError>,
        opened: &OpenedCredentials,
    ) -> Self {
        let tokens = match loaded {
            Ok(Some(tokens)) => tokens,
            Ok(None) => return Self::StoreAbsent,
            Err(StoreError::Corrupt) => return Self::StoreCorrupt,
            Err(StoreError::Backend(e)) => {
                // Unbudgeted on purpose: the page's blob goes with it, and this line is what
                // explains a sign-in screen nothing else accounts for.
                verr!("[AUTH] Secure store unavailable at boot: {e}");
                return Self::StoreUnavailable;
            }
        };
        let store = match opened {
            OpenedCredentials::Opened(store) => store,
            OpenedCredentials::Missing => return Self::BlobUnusable,
            OpenedCredentials::Unreadable => return Self::ReconcileFailed,
        };
        let named: Vec<(&str, BlobNames)> = store
            .blobs()
            .iter()
            .map(|blob| (blob.id.as_str(), BlobNames::of_blob(blob, &tokens)))
            .collect();
        for (_, names) in &named {
            vprintln!("[AUTH]   SDK blob: {names:?}");
        }
        let found = |wanted: BlobNames| named.iter().any(|(_, names)| *names == wanted);
        let names = if found(BlobNames::Mine) {
            BlobNames::Mine
        } else if found(BlobNames::RealTokens) {
            BlobNames::RealTokens
        } else {
            return Self::BlobUnusable;
        };
        let purge = named
            .iter()
            .filter(|(_, names)| !names.stays())
            .map(|(id, _)| (*id).to_owned())
            .collect();
        Self::Restored {
            tokens: Box::new(tokens),
            names,
            purge,
        }
    }

    fn page_blob(&self) -> PageBlob<'_> {
        match self {
            Self::Restored { purge, .. } if purge.is_empty() => PageBlob::Keep,
            Self::Restored { purge, .. } => PageBlob::Purge(purge),
            Self::StoreAbsent
            | Self::StoreCorrupt
            | Self::StoreUnavailable
            | Self::BlobUnusable
            | Self::ReconcileFailed => PageBlob::PurgeAll,
        }
    }
}

/// Decides the boot from the store and Local Storage, then purges what the decision condemns, in
/// one opening of Local Storage.
fn reconcile(loaded: Result<Option<StoredTokenState>, StoreError>, leveldb: &Path) -> BootRestore {
    let opened = sdk_storage::open_credentials(leveldb);
    let restore = BootRestore::decide(loaded, &opened);
    if let OpenedCredentials::Opened(store) = opened {
        let purged = match restore.page_blob() {
            PageBlob::Keep => Purge::Nothing,
            PageBlob::Purge(ids) => store.purge(|blob| ids.contains(&blob.id)),
            PageBlob::PurgeAll => store.purge(|_| true),
        };
        match purged {
            Purge::Nothing => {}
            Purge::Deleted(keys) => vprintln!("[AUTH]   Purged {keys} SDK credential key(s)"),
            Purge::Failed => verr!("[AUTH] The SDK's credentials could not be purged at boot"),
        }
    }
    restore
}

/// Runs before CEF starts: nothing can observe a half-populated AppState.
fn restore_session(restored: StoredTokenState, names: BlobNames) {
    app_state::with_state(|state| {
        state.token_state = Some(restored);
        state.refresh_duty = names.duty();
    });
}

/// The boot's token reconciliation, synchronous and before CEF init: Chromium locks Local Storage
/// once it starts, and the SDK must find its blobs already settled when it first reads them.
pub(crate) fn restore_at_boot(data_dir: &Path, cef_profile: &Path) {
    let leveldb = cef_profile.join("Local Storage").join("leveldb");
    let why = match reconcile(secure_store::load(data_dir), &leveldb) {
        BootRestore::Restored { tokens, names, .. } => {
            vprintln!("[AUTH]   Boot restore: {names:?}");
            restore_session(*tokens, names);
            return;
        }
        BootRestore::StoreAbsent => "no secure store",
        BootRestore::StoreCorrupt => "secure store corrupt",
        BootRestore::StoreUnavailable => "secure store unavailable",
        BootRestore::BlobUnusable => "no SDK blob of this store",
        BootRestore::ReconcileFailed => "Local Storage unreadable",
    };
    vprintln!("[AUTH]   Boot restore: no session ({why})");
}

#[cfg(test)]
#[path = "../tests/unit/boot_tokens.rs"]
mod tests;
