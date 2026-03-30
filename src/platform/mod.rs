pub(crate) mod auth;
pub(crate) mod js_actions;
pub(crate) mod media_controls;
#[allow(dead_code)] // wired in a later commit
pub(crate) mod sdk_storage;
#[allow(dead_code)] // wired in a later commit
pub(crate) mod secure_store;
#[cfg(target_os = "windows")]
pub(crate) mod thumbbar;
pub(crate) mod tray;
#[cfg(target_os = "windows")]
pub(crate) mod volume_sync;
