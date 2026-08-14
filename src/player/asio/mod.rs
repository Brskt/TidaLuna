//! ASIO output backend (Windows only).
//!
//! The COM interface, registry-based driver enumeration, and the real-time host
//! live in Windows-only submodules. PCM conversion is pure and platform-independent;
//! it lives in `convert` and is unit-tested off Windows.

pub(crate) mod convert;
pub(crate) mod driver;

#[cfg(target_os = "windows")]
pub(crate) mod host;
#[cfg(target_os = "windows")]
pub(crate) mod iasio;
