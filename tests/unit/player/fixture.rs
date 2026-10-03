//! What the player tests share, attached to `src/player/mod.rs` by `#[path]`.

use super::Player;

/// A player whose events go nowhere, on this test's runtime, which it builds its governor on.
pub(super) fn silent_player() -> Player {
    Player::new(|_| {}, tokio::runtime::Handle::current())
        .expect("construction opens no device and cannot fail off Windows")
}
