pub(crate) mod auth;
pub(crate) mod js_actions;
pub(crate) mod media_controls;
#[cfg(target_os = "windows")]
pub(crate) mod thumbbar;
pub(crate) mod tray;
#[cfg(target_os = "windows")]
pub(crate) mod volume_sync;
