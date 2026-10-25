//! A plugin's manifest name, checked once where it enters and carried as a type from there.
//!
//! The name is whatever a plugin's `package.json` says, and it is at once an identity, a `{name}/`
//! module prefix and a native data dir folder. As a bare `String` each site re-read it its own way
//! (`"."` and `""` named the native root), and one plugin's file access reached every other's data.

use std::path::{Path, PathBuf};

/// npm's bound on a package name.
const MAX_LEN: usize = 214;

/// Stems Windows opens as a device whatever the extension: a folder named after one is no folder.
const DEVICE_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// `name` or `@scope/name`; each part is ASCII alphanumerics, space or `._+-`, starts with an
/// alphanumeric, ends with neither `.` nor a space (Windows drops both), has no `..` and is no
/// device name. ASCII only, so that SQLite's `NOCASE` folds what case-blind file systems fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginName(String);

/// Why a name was refused, worded for the user who installed the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    Empty,
    TooLong,
    Shape,
    Charset,
    FirstChar,
    LastChar,
    DoubleDot,
    DeviceName,
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Rule::Empty => "it is empty",
            Rule::TooLong => return write!(f, "it is longer than {MAX_LEN} bytes"),
            Rule::Shape => "it must be `name` or `@scope/name`",
            Rule::Charset => "only ASCII letters, digits, space, '.', '_', '+' and '-' are allowed",
            Rule::FirstChar => "each part must start with a letter or a digit",
            Rule::LastChar => "a part must not end with '.' or a space",
            Rule::DoubleDot => "'..' is not allowed",
            Rule::DeviceName => "it is a reserved device name on Windows",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub(crate) raw: String,
    pub(crate) rule: Rule,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin name {:?} is not allowed: {}",
            self.raw, self.rule
        )
    }
}

fn check_part(part: &str) -> Result<(), Rule> {
    let bytes = part.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return Err(Rule::Shape);
    };
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'+' | b'-'))
    {
        return Err(Rule::Charset);
    }
    if !first.is_ascii_alphanumeric() {
        return Err(Rule::FirstChar);
    }
    if matches!(last, b'.' | b' ') {
        return Err(Rule::LastChar);
    }
    if part.contains("..") {
        return Err(Rule::DoubleDot);
    }
    // `con .txt` opens the device too: Windows trims the spaces before the extension.
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    if DEVICE_STEMS.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        return Err(Rule::DeviceName);
    }
    Ok(())
}

impl PluginName {
    pub(crate) fn parse(raw: &str) -> Result<Self, Refused> {
        let checked = if raw.is_empty() {
            Err(Rule::Empty)
        } else if raw.len() > MAX_LEN {
            Err(Rule::TooLong)
        } else if let Some(scoped) = raw.strip_prefix('@') {
            match scoped.split_once('/') {
                Some((scope, name)) => check_part(scope).and_then(|()| check_part(name)),
                None => Err(Rule::Shape),
            }
        } else if raw.contains('/') {
            Err(Rule::Shape)
        } else {
            check_part(raw)
        };
        match checked {
            Ok(()) => Ok(Self(raw.to_string())),
            Err(rule) => Err(Refused {
                raw: raw.to_string(),
                rule,
            }),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// This plugin's own folder under `base`: one plain component, or two for `@scope/name`,
    /// never `base` itself and never outside it.
    pub(crate) fn data_dir_under(&self, base: &Path) -> PathBuf {
        base.join(&self.0)
    }
}

impl std::fmt::Display for PluginName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/name.rs"]
mod tests;
