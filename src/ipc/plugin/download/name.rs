/// Characters no filename may carry on Windows, plus the separators that would silently
/// turn a track title into a directory.
const ILLEGAL_NAME_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Longest filename component ext4, APFS and NTFS accept. A byte bound also satisfies NTFS'
/// limit of 255 UTF-16 units, since no UTF-8 byte expands to more than one unit.
const MAX_NAME_BYTES: usize = 255;

/// Windows refuses these as a filename stem whatever the extension.
const RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The extensions `MediaItem.fileExtension` gives a download. Shells, file managers and browsers
/// pick a file's handler by its extension: this decides which program opens what lands here.
const AUDIO_EXTENSIONS: [&str; 2] = ["flac", "m4a"];

pub(in crate::ipc::plugin) fn audio_named(dest: &std::path::Path) -> bool {
    dest.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|a| ext.eq_ignore_ascii_case(a)))
}

/// Sanitize the file name only, leaving the directory as the caller gave it, as upstream
/// does. The name is what carries a track title: an unsanitized `/` turns one into a
/// directory, and a `:` on Windows addresses an NTFS alternate data stream.
///
/// Both the dialog handler and the download derive their ledger key through this; the two must keep
/// agreeing, and a change here that alters the output shifts every key with it.
pub(in crate::ipc::plugin) fn sanitized_destination(
    path: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let raw = std::path::Path::new(path);
    let dir = raw
        .parent()
        .ok_or_else(|| anyhow::anyhow!("destination has no directory: {path}"))?;
    let name = raw
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("destination has no file name: {path}"))?;

    let mut clean: String = name
        .chars()
        .map(|c| {
            if ILLEGAL_NAME_CHARS.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows silently strips a trailing dot or space, which would leave the written file
    // under a different name than the caller was told.
    while clean.ends_with('.') || clean.ends_with(' ') {
        clean.pop();
    }
    let stem = clean.split('.').next().unwrap_or("").to_uppercase();
    if RESERVED_STEMS.contains(&stem.as_str()) {
        clean.insert(0, '_');
    }
    clean = bound_name(&clean);
    // Cutting the stem can expose a trailing dot or space, which Windows strips again.
    while clean.ends_with('.') || clean.ends_with(' ') {
        clean.pop();
    }
    if clean.is_empty() {
        anyhow::bail!("destination file name is empty after sanitizing: {path}");
    }
    Ok(dir.join(clean))
}

/// Cut the stem, keep the extension. Bounded before anything is fetched: length is the same
/// class of refusal as an illegal character, and a late rename failure wastes a whole track.
fn bound_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    // A leading dot names the file, it does not introduce an extension.
    let (stem, ext) = match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    };
    match MAX_NAME_BYTES
        .checked_sub(ext.len())
        .filter(|budget| *budget > 0)
    {
        Some(budget) => format!("{}{ext}", crate::util::truncate_str(stem, budget)),
        // An extension that fills the budget on its own leaves no stem to keep.
        None => crate::util::truncate_str(name, MAX_NAME_BYTES).to_string(),
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/download/name.rs"]
mod tests;
