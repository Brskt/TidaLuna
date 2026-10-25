//! Where the front folder is: beside the executable, or wherever the dev override points.

use std::ffi::OsString;
use std::path::PathBuf;

/// Points a launch at another front folder, a checkout's `frontend/dist` for instance. Read once
/// at launch, never from a setting the app or a plugin could change.
pub(crate) const OVERRIDE_ENV: &str = "TIDALUNAR_FRONTEND_DIR";

/// macOS keeps read-only app data in the bundle's `Resources`, beside `Contents/MacOS`.
#[cfg(target_os = "macos")]
const FROM_MAIN_EXE: &str = "../Resources/frontend";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirOrigin {
    /// The folder the install shipped.
    Shipped,
    /// The folder `TIDALUNAR_FRONTEND_DIR` named.
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrontendDir {
    pub path: PathBuf,
    pub origin: DirOrigin,
}

#[derive(Debug)]
pub(crate) enum FrontendDirError {
    OverrideNotAbsolute(PathBuf),
    OverrideNotUtf8,
    InstallDirUnknown,
    InstallDirNotText,
}

impl FrontendDirError {
    pub(crate) fn refusal(&self) -> String {
        match self {
            Self::OverrideNotAbsolute(dir) => format!(
                "{OVERRIDE_ENV} names {}, which is not an absolute folder. Set it to an absolute path or unset it.",
                dir.display()
            ),
            Self::OverrideNotUtf8 => {
                format!("{OVERRIDE_ENV} is not valid text. Set it to an absolute path or unset it.")
            }
            Self::InstallDirUnknown => {
                "TidaLunar cannot tell where it is installed and cannot find its front files."
                    .to_string()
            }
            Self::InstallDirNotText => {
                "TidaLunar is installed under a path that is not valid text. Move it to a plain folder."
                    .to_string()
            }
        }
    }
}

/// The override wins when set; a set but unusable one is an error rather than a silent fall back
/// to the shipped folder, which would run a front the developer did not mean to test.
pub(crate) fn resolve_frontend_dir(
    env: Option<OsString>,
    exe: std::io::Result<PathBuf>,
) -> Result<FrontendDir, FrontendDirError> {
    if let Some(raw) = env {
        let path = PathBuf::from(raw);
        if path.to_str().is_none() {
            return Err(FrontendDirError::OverrideNotUtf8);
        }
        if !path.is_absolute() {
            return Err(FrontendDirError::OverrideNotAbsolute(path));
        }
        return Ok(FrontendDir {
            path,
            origin: DirOrigin::Override,
        });
    }
    shipped_dir(exe).map(|path| FrontendDir {
        path,
        origin: DirOrigin::Shipped,
    })
}

#[cfg(not(target_os = "macos"))]
fn shipped_dir(exe: std::io::Result<PathBuf>) -> Result<PathBuf, FrontendDirError> {
    match crate::state::install_dir(exe) {
        crate::state::InstallDir::Found(dir) => Ok(dir.join("frontend")),
        crate::state::InstallDir::Unknown => Err(FrontendDirError::InstallDirUnknown),
        crate::state::InstallDir::NotText => Err(FrontendDirError::InstallDirNotText),
    }
}

#[cfg(target_os = "macos")]
fn shipped_dir(exe: std::io::Result<PathBuf>) -> Result<PathBuf, FrontendDirError> {
    let exe = exe.map_err(|_| FrontendDirError::InstallDirUnknown)?;
    if exe.to_str().is_none() {
        return Err(FrontendDirError::InstallDirNotText);
    }
    let dir = exe.parent().ok_or(FrontendDirError::InstallDirUnknown)?;
    Ok(dir.join(FROM_MAIN_EXE))
}

/// After the update recovery, which may roll the shipped folder back.
pub(crate) fn frontend_dir(_: &crate::updater::Recovered) -> Result<FrontendDir, FrontendDirError> {
    resolve_frontend_dir(std::env::var_os(OVERRIDE_ENV), std::env::current_exe())
}

#[cfg(test)]
#[path = "../../tests/unit/frontend/dir.rs"]
mod tests;
