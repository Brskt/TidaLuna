//! Tells the player thread that the set of output devices changed, for it to hand the page a fresh
//! list. The official engine re-lists on a device added, removed or made active; a disable alone
//! lists nothing, and the selected device's loss is reported by its stream, not here.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::{Watch as DeviceWatch, start};
#[cfg(target_os = "macos")]
pub(crate) use macos::{Watch as DeviceWatch, start};
#[cfg(target_os = "windows")]
pub(crate) use windows::{Watch as DeviceWatch, start};

/// Runs on the OS's notification thread: it only queues a command, never blocks.
pub(crate) type Signal = std::sync::Arc<dyn Fn() + Send + Sync>;
