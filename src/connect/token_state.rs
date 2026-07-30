//! Linearized auth token state for TIDAL Connect.
//!
//! A single `TokenState` bundles everything that belongs to a coherent
//! authentication context: the access token itself, the refresh token, the
//! scope, the expiry, and where the generation sits in its lifecycle. All
//! updates replace the bundle atomically via `ArcSwap::compare_and_swap`,
//! so there is no window in which two refresh attempts can interleave and
//! persist torn state (e.g. one task writing a new access token while
//! another overwrites the refresh token).
//!
//! The choice of `ArcSwap` over a `Mutex` is deliberate: reads dominate
//! (every HTTP request snapshots the current token), writes are rare
//! (only on refresh / login / logout), and `ArcSwap::load` is lock-free
//! and fast. The immutable snapshot is also the correct input to the CAS
//! on write, making the "read current, try to write" pattern natural.

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;

// ---------------------------------------------------------------------------
// Lifecycle state
// ---------------------------------------------------------------------------

/// Where a generation sits in its lifecycle.
///
/// Currently only the terminal/non-terminal distinction is wired: `Active`
/// is the normal state, `Terminated(reason)` locks the generation against
/// further refresh attempts. Additional intermediate states (Refreshing,
/// RefreshFailed, Suspended) can be introduced once a real call site needs
/// them; keeping them out for now avoids shipping a vocabulary that no
/// code uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationStatus {
    /// Normal operating state. The token is valid and usable.
    Active,
    /// The generation can no longer be used. The user must relogin.
    Terminated(TerminationReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationReason {
    /// RFC 6749 invalid_grant or equivalent. `provider_error` preserves the
    /// upstream diagnostic so the UI can show it. `suspect_replay` is an
    /// observational flag: the client cannot reliably detect replay by
    /// itself (RFC 9700 §4.14.2), so it is set from heuristics only and
    /// never on its own terminates anything.
    InvalidGrant {
        provider_error: String,
        suspect_replay: bool,
    },
    /// RFC 7009 revocation observed on the server side (401 on a request
    /// made with a freshly-minted access token).
    Revoked,
}

// ---------------------------------------------------------------------------
// Token bundle
// ---------------------------------------------------------------------------

/// Immutable snapshot of the authentication state. Stored behind an
/// `ArcSwap` so reads are lock-free and writes are atomic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenState {
    /// Monotonic generation id. Incremented on login / relogin. Used to
    /// distinguish tokens that belong to different sign-ins when a stale
    /// IPC message arrives late.
    pub generation: u64,
    /// Per-generation token version. Incremented on every successful refresh
    /// within a generation.
    pub token_version: u64,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    pub expires_at: Instant,
    pub status: GenerationStatus,
}

// ---------------------------------------------------------------------------
// CAS errors
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum CASError {
    /// Another writer changed the state between the snapshot and the CAS.
    /// The caller should re-read and retry if the new state still permits
    /// the intended update.
    VersionMismatch,
    /// The generation is `Terminated`. Applying a refresh to a terminated
    /// generation is a protocol error; the caller must relogin.
    Terminated,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Lock-free store for the current `TokenState`. All updates go through
/// `compare_and_swap` (either directly or via a `RefreshGuard`), so a
/// concurrent writer can never persist half of an update.
pub struct AuthStore {
    inner: ArcSwap<TokenState>,
}

impl AuthStore {
    pub fn new(initial: TokenState) -> Self {
        Self {
            inner: ArcSwap::from(Arc::new(initial)),
        }
    }

    /// Current snapshot. Cheap: a single atomic load plus an Arc clone.
    pub fn load(&self) -> Arc<TokenState> {
        self.inner.load_full()
    }

    /// Atomically replace the state when the current value is the one
    /// `expected` points at. This rejects stale writes (another writer got
    /// there first) and refuses any write against a `Terminated` generation.
    ///
    /// Returns `Ok(())` only when the snapshot was still current AND the
    /// transition away from that snapshot is legal.
    pub fn compare_and_swap(
        &self,
        expected: &Arc<TokenState>,
        new: TokenState,
    ) -> Result<(), CASError> {
        if matches!(expected.status, GenerationStatus::Terminated(_)) {
            return Err(CASError::Terminated);
        }
        let previous = self.inner.compare_and_swap(expected, Arc::new(new));
        if Arc::ptr_eq(&previous, expected) {
            Ok(())
        } else {
            Err(CASError::VersionMismatch)
        }
    }

    /// Unconditionally replace the state. Used when the authority for the
    /// auth context is upstream (e.g. the mobile client pushes a fresh
    /// `ServerInfo` after a relogin): a previously-Terminated generation
    /// must not block the new tokens from being installed.
    ///
    /// Callers MUST advance `generation` past the current snapshot; this
    /// method does not check ordering, so the caller is responsible for
    /// making the replacement monotonically correct.
    pub fn store(&self, new: TokenState) {
        self.inner.store(Arc::new(new));
    }
}

// ---------------------------------------------------------------------------
// Refresh guard
// ---------------------------------------------------------------------------

/// Helper that captures a snapshot at construction time and performs a CAS
/// against that snapshot on `try_apply`. This is the pattern every refresh
/// path should use:
///
/// ```text
/// let guard = RefreshGuard::new(&store);
/// let new_state = build_new_state(&guard.snapshot());
/// guard.try_apply(new_state)?;
/// ```
///
/// If another writer wins the race between `new` and `try_apply`, the CAS
/// fails with `VersionMismatch` and the caller can re-read and decide what
/// to do (retry, abandon, or escalate to an error).
pub struct RefreshGuard<'a> {
    store: &'a AuthStore,
    snapshot: Arc<TokenState>,
}

impl<'a> RefreshGuard<'a> {
    pub fn new(store: &'a AuthStore) -> Self {
        Self {
            snapshot: store.load(),
            store,
        }
    }

    pub fn snapshot(&self) -> &Arc<TokenState> {
        &self.snapshot
    }

    pub fn try_apply(&self, new: TokenState) -> Result<(), CASError> {
        self.store.compare_and_swap(&self.snapshot, new)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "../../tests/unit/connect/token_state.rs"]
mod tests;
