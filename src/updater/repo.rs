//! The repository releases come from.
//!
//! Shared by `#[path]` with the `updater` crate and xtask: keep this file free of anything from the
//! app crate, and of anything one of the three would not use (an unused item fails its lint).

pub(crate) const GITHUB_OWNER: &str = "Brskt";
pub(crate) const GITHUB_REPO: &str = "TidaLuna";
