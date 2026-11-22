pub(crate) const PLAY_PAUSE: &str = "window.__TL_PLAY_PAUSE__?.()";
pub(crate) const PLAY_NEXT: &str = "window.__TIDAL_PLAYBACK_DELEGATE__?.playNext?.()";
pub(crate) const PLAY_PREV: &str = "window.__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.()";
/// Pinned by the page once its store is up (`frontend/src/controllers/logout.ts`). Before that,
/// the click only leaves a warning: there is no session to sign out of yet.
pub(crate) const LOGOUT: &str = "window.__TL_LOGOUT__ ? window.__TL_LOGOUT__() : console.warn('[menu] Log Out: the TIDAL page is not ready')";
