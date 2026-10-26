//! Reads the front folder once, at launch.
//!
//! One snapshot for the whole run: the updater replaces files in place while the app runs, and a
//! page reload must not mix the bundle of one release with the modules of the next. Read before
//! CEF starts; no CEF thread ever waits on the disk.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::dir::{DirOrigin, FrontendDir, OVERRIDE_ENV};
use super::files::{BUNDLE, LUNA_DEV, LUNA_UI, MANIFEST, NATIVE_HOST};

/// Declares the early-runtime scripts from one list of names: each field holds `<field>.js`, never
/// another script's file. Tests also get `EARLY_RUNTIME_FILES`; a script left off the list fails
/// `the_early_runtime_list_matches_the_source_folder`.
macro_rules! early_runtime {
    ($($field:ident),* $(,)?) => {
        #[cfg(test)]
        pub(crate) const EARLY_RUNTIME_FILES: [&str; [$(stringify!($field)),*].len()] =
            [$(concat!(stringify!($field), ".js")),*];

        pub(crate) struct EarlyRuntime {
            $(pub $field: Box<str>,)*
        }

        impl EarlyRuntime {
            fn read(dir: &Path) -> Result<Self, FrontendError> {
                Ok(Self {
                    $($field: script(&dir.join(concat!(stringify!($field), ".js")))?,)*
                })
            }
        }
    };
}

early_runtime! {
    host_modules,
    saga_put,
    ipc,
    volume_restore,
    token_capture,
    fetch_proxy,
    window_open,
    session_stub,
    exfil_guard,
    plugin_loads,
    fallback_titlebar,
    cache_heal,
    init_static,
}

/// Bun's bootstrap stops reading the host script at a line holding exactly this
/// (`native_runtime::NativeRuntime::BOOTSTRAP`); the lines after it would reach the host as IPC.
const END_MARKER: &str = "__END__";

pub(crate) struct Frontend {
    pub bundle: Box<str>,
    pub luna_ui: Box<[u8]>,
    pub luna_dev: Box<[u8]>,
    pub native_host: Box<str>,
    pub early: EarlyRuntime,
}

#[derive(Debug)]
pub(crate) enum FrontendError {
    Missing(PathBuf),
    Unreadable(PathBuf, std::io::Error),
    NotUtf8(PathBuf),
    Empty(PathBuf),
    EndMarker(PathBuf),
    BadManifest(PathBuf),
    VersionMismatch {
        found: String,
        expected: &'static str,
    },
}

impl FrontendError {
    pub(crate) fn refusal(&self, dir: &FrontendDir) -> String {
        let what = match self {
            Self::Missing(p) => format!("The front file {} is missing.", p.display()),
            Self::Unreadable(p, e) => {
                format!("The front file {} cannot be read: {e}.", p.display())
            }
            Self::NotUtf8(p) => format!("The front file {} is not valid UTF-8 text.", p.display()),
            Self::Empty(p) => format!("The front file {} is empty.", p.display()),
            Self::EndMarker(p) => format!(
                "The front file {} holds a line reading {END_MARKER}, which would cut the script short.",
                p.display()
            ),
            Self::BadManifest(p) => format!("The front file {} names no version.", p.display()),
            Self::VersionMismatch { found, expected } => {
                format!("The front files are from version {found}, this build is {expected}.")
            }
        };
        let remedy = match dir.origin {
            // A source checkout lands here too: `cargo run` has no front beside its exe.
            DirOrigin::Shipped => format!(
                "Reinstall TidaLunar, or unpack the release again. From a source checkout, set {OVERRIDE_ENV} to the absolute path of frontend/dist."
            ),
            DirOrigin::Override => format!(
                "{OVERRIDE_ENV} points at {}: rebuild the front there, or unset it.",
                dir.path.display()
            ),
        };
        format!("{what} {remedy}")
    }
}

static FRONTEND: OnceLock<Frontend> = OnceLock::new();

pub(crate) fn install(front: Frontend) {
    if FRONTEND.set(front).is_err() {
        crate::verr!("[FRONT]  The front was loaded twice; keeping the first copy");
    }
}

pub(crate) fn get() -> Option<&'static Frontend> {
    FRONTEND.get()
}

pub(crate) fn load(dir: &FrontendDir) -> Result<Frontend, FrontendError> {
    check_version(dir, &dir.path.join(MANIFEST))?;
    let host_path = dir.path.join(NATIVE_HOST);
    let native_host = script(&host_path)?;
    // Readline also ends a line at a lone carriage return.
    if native_host
        .split(['\n', '\r'])
        .any(|line| line == END_MARKER)
    {
        return Err(FrontendError::EndMarker(host_path));
    }
    let bundle = script(&dir.path.join(BUNDLE))?;
    let luna_ui = bytes(&dir.path.join(LUNA_UI))?.into_boxed_slice();
    let luna_dev = bytes(&dir.path.join(LUNA_DEV))?.into_boxed_slice();
    let early = EarlyRuntime::read(&dir.path.join("early-runtime"))?;
    Ok(Frontend {
        bundle,
        luna_ui,
        luna_dev,
        native_host,
        early,
    })
}

/// A shipped folder from another build is refused; an overridden one is the developer's choice,
/// told once in the log.
fn check_version(dir: &FrontendDir, path: &Path) -> Result<(), FrontendError> {
    let raw = bytes(path)?;
    let found = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("version")?.as_str().map(str::to_string))
        .ok_or_else(|| FrontendError::BadManifest(path.to_path_buf()))?;
    let expected = env!("CARGO_PKG_VERSION");
    if found == expected {
        return Ok(());
    }
    match dir.origin {
        DirOrigin::Shipped => Err(FrontendError::VersionMismatch { found, expected }),
        DirOrigin::Override => {
            crate::verr!(
                "[FRONT]  {} holds front version {found}, this build is {expected}",
                dir.path.display()
            );
            Ok(())
        }
    }
}

fn bytes(path: &Path) -> Result<Vec<u8>, FrontendError> {
    let data = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => FrontendError::Missing(path.to_path_buf()),
        _ => FrontendError::Unreadable(path.to_path_buf(), e),
    })?;
    if data.is_empty() {
        return Err(FrontendError::Empty(path.to_path_buf()));
    }
    Ok(data)
}

fn script(path: &Path) -> Result<Box<str>, FrontendError> {
    let data = bytes(path)?;
    String::from_utf8(data)
        .map(String::into_boxed_str)
        .map_err(|_| FrontendError::NotUtf8(path.to_path_buf()))
}

#[cfg(test)]
#[path = "../../tests/unit/frontend/load.rs"]
mod tests;
