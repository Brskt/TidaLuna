use super::{FailureClass, off_runtime};

/// One shipped platform. No single format string fits the three families: Windows drops the
/// `lib` prefix, and only Linux carries a SONAME infix. `os` and `arch` are `std::env::consts`
/// spellings.
pub(crate) struct CodecTarget {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) file_name: &'static str,
}

impl CodecTarget {
    /// Where Cisco publishes this build.
    pub(crate) fn url(&self) -> String {
        format!("https://{CODEC_HOST}/{}.bz2", self.file_name)
    }
}

/// OpenH264 2.6.0, the version `openh264` 0.9.8 is built against.
///
/// These digests are the second block of `openh264-sys2`'s `hashes.txt`, kept here rather than
/// deferred to that crate: its first block assigns one identical digest to eight different
/// architectures, which is impossible, and widening the accepted set costs nothing to an
/// attacker. `DynamicAPI::from_blob_path` still checks against its own table afterwards, so
/// this is the first line and that one is the second.
pub(crate) const TARGETS: &[CodecTarget] = &[
    CodecTarget {
        os: "windows",
        arch: "x86_64",
        sha256: "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691",
        file_name: "openh264-2.6.0-win64.dll",
    },
    CodecTarget {
        os: "windows",
        arch: "aarch64",
        sha256: "fb75103938f4f47d119b983e06334df41a803bc72fb5c46e3623f6fea5782732",
        file_name: "openh264-2.6.0-win-arm64.dll",
    },
    CodecTarget {
        os: "linux",
        arch: "x86_64",
        sha256: "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
        file_name: "libopenh264-2.6.0-linux64.8.so",
    },
    CodecTarget {
        os: "linux",
        arch: "aarch64",
        sha256: "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
        file_name: "libopenh264-2.6.0-linux-arm64.8.so",
    },
    CodecTarget {
        os: "macos",
        arch: "x86_64",
        sha256: "e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19",
        file_name: "libopenh264-2.6.0-mac-x64.dylib",
    },
    CodecTarget {
        os: "macos",
        arch: "aarch64",
        sha256: "052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551",
        file_name: "libopenh264-2.6.0-mac-arm64.dylib",
    },
];

/// `None` where Cisco publishes no build we ship against: the feature is then `Unsupported`.
pub(crate) fn current_target() -> Option<&'static CodecTarget> {
    TARGETS
        .iter()
        .find(|t| t.os == std::env::consts::OS && t.arch == std::env::consts::ARCH)
}

/// The one host this feature may reach, by exact equality: a suffix or substring test admits a
/// registrable lookalike. Its own predicate rather than an addition to `nav::is_tidal_api_host`
/// or `download::net::allowed_url`, which would widen those for every caller that trusts them.
fn is_codec_host(host: &str) -> bool {
    host == CODEC_HOST
}

const CODEC_HOST: &str = "ciscobinary.openh264.org";

/// HTTPS only, with the host taken from the parsed url rather than the string. Checked again at
/// every redirect hop.
fn codec_redirect_allowed(url: &url::Url) -> bool {
    url.scheme() == "https" && is_codec_host(url.host_str().unwrap_or(""))
}

fn codec_url_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| codec_redirect_allowed(&parsed))
}

/// A download whose digest does not match `TARGETS`, carried by `FetchError::Digest`.
#[derive(Debug)]
pub(super) struct CodecDigestMismatch {
    expected: &'static str,
    found: String,
}

impl std::fmt::Display for CodecDigestMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "codec digest mismatch: expected {}, found {}",
            self.expected, self.found
        )
    }
}

/// Why an install failed, tagged by the STAGE that failed. No `From`: every `?` in
/// `fetch_and_install` names its stage; a new local step cannot fall by default into the class
/// that retries, the way an `io::Error` from a full disk would look like one from the network.
#[derive(Debug)]
pub(super) enum FetchError {
    /// The url is not on the codec host.
    OffHost,
    /// Cisco publishes no build for this platform.
    NoTarget,
    /// Anything before a status line: DNS, connect, TLS, timeout.
    Unreachable(reqwest::Error),
    /// A redirect the codec policy refused.
    Redirected(reqwest::Error),
    Status(reqwest::StatusCode),
    /// The body broke off after the status line.
    Body(reqwest::Error),
    TooLarge,
    /// The bzip2 stream, or its inflate past the ceiling.
    Archive(anyhow::Error),
    Digest(CodecDigestMismatch),
    /// The directory, the temporary file, the write, the permissions or the rename.
    Save(std::io::Error),
    /// The runtime ended the blocking task, which only happens while it shuts down.
    Shutdown,
}

impl FetchError {
    /// Read off the stage that failed, never off the text. `None` for a shutdown: nothing failed,
    /// and nobody is left to tell.
    pub(super) fn class(&self) -> Option<FailureClass> {
        Some(match self {
            Self::Unreachable(_) | Self::Body(_) => FailureClass::Network,
            Self::Status(s) => FailureClass::of_status(*s),
            Self::OffHost | Self::NoTarget | Self::Redirected(_) => FailureClass::Refused,
            Self::TooLarge | Self::Archive(_) | Self::Digest(_) => FailureClass::Corrupt,
            Self::Save(_) => FailureClass::Disk,
            Self::Shutdown => return None,
        })
    }
}

/// The whole cause, for the log only: `NoDecoder::phrase` is what travels to the page.
impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OffHost => f.write_str("codec url is not on the codec host"),
            Self::NoTarget => f.write_str("no codec build is published for this platform"),
            Self::Unreachable(e) => write_chain(f, "sending the codec request failed", e),
            Self::Redirected(e) => write_chain(f, "the codec host redirected elsewhere", e),
            Self::Status(s) => write!(f, "codec host answered {s}"),
            Self::Body(e) => write_chain(f, "reading the codec archive failed", e),
            Self::TooLarge => write!(
                f,
                "codec archive exceeds the ceiling of {MAX_CODEC_BYTES} bytes"
            ),
            Self::Archive(e) => write!(f, "the codec archive did not inflate: {e:#}"),
            Self::Digest(e) => e.fmt(f),
            Self::Save(e) => write!(f, "saving the codec failed: {e}"),
            Self::Shutdown => f.write_str("the runtime shut down before the codec was installed"),
        }
    }
}

/// No `source()`: `Display` already carries the cause, and `{:#}` would print it twice.
impl std::error::Error for FetchError {}

/// `context: e: its source: ...`, the shape `{:#}` gives an `anyhow::Error`. reqwest's own
/// `Display` stops at the top, which says "error sending request" and never why.
pub(crate) fn write_chain(
    f: &mut std::fmt::Formatter<'_>,
    context: &str,
    e: &dyn std::error::Error,
) -> std::fmt::Result {
    write!(f, "{context}: {e}")?;
    let mut cause = e.source();
    while let Some(c) = cause {
        write!(f, ": {c}")?;
        cause = c.source();
    }
    Ok(())
}

/// The codec download's own client. Not `DOWNLOAD_CLIENT`, whose redirect policy refuses
/// anything outside TIDAL's hosts; not `HTTP_CLIENT`, which follows redirects with no policy and
/// carries a shared cookie store this third-party host has no business seeing (this one has none).
static CODEC_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::state::build_host_checked_client(codec_redirect_allowed, "redirect left the codec host")
});

use std::path::PathBuf;

/// Ceiling on the downloaded archive and on its inflate, whose output size a bzip2 stream never
/// declares.
const MAX_CODEC_BYTES: usize = 16 * 1024 * 1024;

/// One request, one small file. `base_client_builder` sets no read timeout on purpose, so
/// this path sets its own: a third-party host that accepts the connection and then says
/// nothing would otherwise hold the task for the life of the process.
const CODEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// `<data_dir>/codec`.
fn codec_dir() -> PathBuf {
    crate::state::cache_data_dir().join("codec")
}

pub(super) fn installed_path(target: &CodecTarget) -> PathBuf {
    codec_dir().join(target.file_name)
}

pub(crate) fn sha256_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    base16ct::lower::encode_string(&hasher.finalize())
}

/// Inflate, refusing to grow past `max`. The check is inside the read loop because a bzip2
/// stream carries no declared output length: reading it whole and measuring afterwards has
/// already made the allocation the ceiling exists to prevent.
fn decompress_capped(data: &[u8], max: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;

    let mut decoder = bzip2::read::BzDecoder::new(data);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() + n > max {
            anyhow::bail!("decompressed codec exceeds the ceiling of {max} bytes");
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Download, inflate, verify, install.
///
/// Order is load-bearing: the digest covers the decompressed file, and nothing is written to
/// the destination name until it has matched. A mismatched download leaves no file behind, so
/// a later run retries rather than loading a blob that was rejected once.
pub(super) async fn fetch_and_install(target: &'static CodecTarget) -> Result<PathBuf, FetchError> {
    let url = target.url();
    if !codec_url_allowed(&url) {
        crate::vprintln!(
            "[VIDEO]   Refused a codec url outside the codec host: {}",
            crate::util::refused_host(&url)
        );
        return Err(FetchError::OffHost);
    }

    let packed = download_archive(&url).await?;

    let dest = installed_path(target);
    let expected = target.sha256;
    off_runtime(move || -> Result<PathBuf, FetchError> {
        let plain = decompress_capped(&packed, MAX_CODEC_BYTES).map_err(FetchError::Archive)?;
        let found = sha256_bytes(&plain);
        if found != expected {
            return Err(FetchError::Digest(CodecDigestMismatch { expected, found }));
        }
        write_atomically(&dest, &plain).map_err(FetchError::Save)?;
        Ok(dest)
    })
    .await
    .unwrap_or(Err(FetchError::Shutdown))
}

async fn download_archive(url: &str) -> Result<Vec<u8>, FetchError> {
    let resp = CODEC_CLIENT
        .get(url)
        .timeout(CODEC_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            // The policy's refusal surfaces as a send error too; only this flag tells it apart
            // from a host that never answered.
            if e.is_redirect() {
                FetchError::Redirected(e)
            } else {
                FetchError::Unreachable(e)
            }
        })?;
    if !resp.status().is_success() {
        return Err(FetchError::Status(resp.status()));
    }

    let mut packed = Vec::new();
    let mut stream = resp.bytes_stream();
    {
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(FetchError::Body)?;
            if packed.len() + chunk.len() > MAX_CODEC_BYTES {
                return Err(FetchError::TooLarge);
            }
            packed.extend_from_slice(&chunk);
        }
    }
    Ok(packed)
}

/// Writes `bytes` to a temporary file beside `dest`, syncs it, then renames it onto `dest`: a
/// reader sees the old file or the whole new one. Every error here is local.
fn write_atomically(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let dir = dest
        .parent()
        .ok_or_else(|| std::io::Error::other("codec destination has no directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    persist_outlasting_holds(tmp, dest)
}

/// Renames the synced `tmp` onto `dest`, waiting out another program that holds either for a
/// moment (see `LOCK_BUDGET`).
pub(crate) fn persist_outlasting_holds(
    mut tmp: tempfile::NamedTempFile,
    dest: &Path,
) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let mut pause = LOCK_FIRST_PAUSE;
    loop {
        match tmp.persist(dest) {
            Ok(_) => return Ok(()),
            Err(failed) if wait_if_locked(&failed.error, start, &mut pause) => tmp = failed.file,
            Err(failed) => return Err(failed.error),
        }
    }
}

/// How long a rename waits out another program holding the file. A scanner or indexer opens a
/// freshly written file for a moment, and Windows then refuses the rename; Go's `robustio` and
/// LLVM's `Path.inc` give it the same two seconds before calling the failure real.
const LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
const LOCK_FIRST_PAUSE: std::time::Duration = std::time::Duration::from_millis(1);
const LOCK_PAUSE_CAP: std::time::Duration = std::time::Duration::from_millis(100);

/// ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32), how Windows says another program has
/// the file open. Elsewhere a rename does not fail that way.
fn is_held_by_another_program(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32))
}

/// Sleeps and answers true when `e` is a hold another attempt can outlast within `LOCK_BUDGET`.
fn wait_if_locked(
    e: &std::io::Error,
    start: std::time::Instant,
    pause: &mut std::time::Duration,
) -> bool {
    if !is_held_by_another_program(e) || start.elapsed() + *pause >= LOCK_BUDGET {
        return false;
    }
    std::thread::sleep(*pause);
    *pause = (*pause * 2).min(LOCK_PAUSE_CAP);
    true
}

use std::path::Path;

/// Load the library and prove it is the one we think it is.
///
/// `from_blob_path` checks the file against `openh264-sys2`'s baked digest table, which we
/// treat as a second line rather than the first (see `TARGETS`). The probe after it is the
/// part that matters here: `APILoader` keeps every symbol as a `Result` and unwraps it with
/// `.expect(...)` at the call site, so a file that `dlopen`s but exports no Wels symbols loads
/// without error and then panics inside a decode. Asking for the codec version once, here,
/// turns that into an ordinary `Err`.
pub(crate) fn load_api(path: &Path) -> anyhow::Result<openh264::OpenH264API> {
    let api = openh264::OpenH264API::from_blob_path(path)
        .map_err(|e| anyhow::anyhow!("loading the video codec failed: {e:?}"))?;

    let probed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use openh264_sys2::API;
        // SAFETY: no arguments. A missing symbol panics inside the loader, which `catch_unwind`
        // catches; a present one is OpenH264's, the file having passed `from_blob_path`'s digest
        // check.
        unsafe { api.WelsGetCodecVersion() }
    }))
    .map_err(|_| anyhow::anyhow!("the video codec library is missing its entry points"))?;

    if probed.uMajor == 0 && probed.uMinor == 0 && probed.uRevision == 0 {
        anyhow::bail!("the video codec library reported no version");
    }
    Ok(api)
}

/// What the file at the install path is, once looked at.
#[derive(Debug)]
pub(super) enum Verified {
    Installed(PathBuf),
    Absent,
    /// A file that failed its digest, deleted by this call.
    Removed,
    /// A file that can be neither read nor, when wrong, removed: whatever blocks it blocks a
    /// replacement the same way; a download would only repeat the failure.
    Stuck(std::io::Error),
}

/// Hashes `path` against `target.sha256`, apart from the supervisor's IO for tests to drive on a
/// temp file. Re-hashed at every verify rather than trusted once: a file at `installed_path`
/// persists across runs and can have been swapped, corrupted, or left from another platform, and
/// `from_blob_path`'s own table accepts a wrong-architecture build (see `TARGETS`).
///
/// Only bytes actually hashed can answer `Installed`, and only `NotFound` is `Absent`: any
/// other read error (another program holding the file, a directory in its place) is `Stuck`.
pub(super) fn verify_installed(path: &Path, target: &CodecTarget) -> Verified {
    verify_installed_with(path, target, |p| std::fs::remove_file(p))
}

/// [`verify_installed`] with the removal of a mismatched file supplied, for tests to fail it.
fn verify_installed_with(
    path: &Path,
    target: &CodecTarget,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Verified {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Verified::Absent,
        Err(e) => return Verified::Stuck(e),
    };
    if sha256_bytes(&data) == target.sha256 {
        return Verified::Installed(path.to_path_buf());
    }
    match remove(path) {
        Ok(()) => {
            crate::verr!("[VIDEO]   Codec digest mismatch, removed the installed file");
            Verified::Removed
        }
        // Another caller removed it between our read and our remove: the same end state.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Verified::Removed,
        Err(e) => Verified::Stuck(e),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/video/codec/install.rs"]
mod tests;
