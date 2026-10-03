//! Obtaining Cisco's prebuilt OpenH264 library.
//!
//! The library is downloaded after install rather than bundled, because Cisco's AVC patent
//! coverage attaches to the binary Cisco itself builds and not to source anyone else compiles.
//! The four conditions that coverage carries are load-bearing here: separate download, a user
//! switch, the attribution string beside that switch, and Cisco's full notice with condition 4
//! wherever licensing information is shown.

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
/// or `download::allowed_url`, which would widen those for every caller that trusts them.
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

/// A download whose digest does not match `TARGETS`. A type, not a message: `install_failure`
/// finds it in `e.chain()`, and no `.context()` added on the way can reroute a poisoned blob into
/// `DownloadFailed`, the variant that invites a retry.
#[derive(Debug)]
struct CodecDigestMismatch {
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

impl std::error::Error for CodecDigestMismatch {}

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

fn installed_path(target: &CodecTarget) -> PathBuf {
    codec_dir().join(target.file_name)
}

fn sha256_bytes(data: &[u8]) -> String {
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
async fn fetch_and_install(target: &'static CodecTarget) -> anyhow::Result<PathBuf> {
    let url = target.url();
    if !codec_url_allowed(&url) {
        crate::vprintln!(
            "[VIDEO]   Refused a codec url outside the codec host: {}",
            crate::util::refused_host(&url)
        );
        return Err(crate::util::Refused("codec url is not on the codec host").into());
    }

    let resp = CODEC_CLIENT.get(&url).timeout(CODEC_TIMEOUT).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("codec host answered {}", resp.status());
    }

    let mut packed = Vec::new();
    let mut stream = resp.bytes_stream();
    {
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if packed.len() + chunk.len() > MAX_CODEC_BYTES {
                anyhow::bail!("codec archive exceeds the ceiling of {MAX_CODEC_BYTES} bytes");
            }
            packed.extend_from_slice(&chunk);
        }
    }

    let dest = installed_path(target);
    let expected = target.sha256;
    tokio::task::spawn_blocking(move || -> anyhow::Result<PathBuf> {
        use std::io::Write;

        let plain = decompress_capped(&packed, MAX_CODEC_BYTES)?;
        let found = sha256_bytes(&plain);
        if found != expected {
            return Err(CodecDigestMismatch { expected, found }.into());
        }

        let dir = dest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("codec destination has no directory"))?;
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&plain)?;
        tmp.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
        }
        tmp.persist(&dest)?;
        Ok(dest)
    })
    .await?
}

use std::path::Path;

/// What this build can say about its decoder. A named enum: a bool would collapse "the user
/// turned it off" into "it failed", and those want different words in front of the user and
/// different offers beside them.
#[derive(Debug, Clone)]
pub(crate) enum CodecStatus {
    Ready(PathBuf),
    Disabled,
    NotDownloaded,
    DownloadFailed(String),
    DigestMismatch,
    Unsupported,
}

impl CodecStatus {
    /// No production caller yet: the cover transport reads `CodecStatus` only for `reason()`.
    /// `expect`, not `allow`, so `cargo xtask clippy` (`-D warnings`) fails the day a caller
    /// arrives; gated to non-test builds because the unit test is that caller under `cargo test`.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Ready(p) => Some(p.as_path()),
            Self::Disabled
            | Self::NotDownloaded
            | Self::DownloadFailed(_)
            | Self::DigestMismatch
            | Self::Unsupported => None,
        }
    }

    /// A short phrase for the UI, not a log line: each variant says what happened and, where it
    /// applies, what the user can do.
    pub(crate) fn reason(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Ready(_) => std::borrow::Cow::Borrowed("ready"),
            Self::Disabled => {
                std::borrow::Cow::Borrowed("video decoding is turned off in settings")
            }
            Self::NotDownloaded => {
                std::borrow::Cow::Borrowed("the video codec has not been downloaded yet")
            }
            Self::DownloadFailed(cause) => {
                std::borrow::Cow::Owned(format!("the video codec could not be downloaded: {cause}"))
            }
            Self::DigestMismatch => {
                std::borrow::Cow::Borrowed("the downloaded video codec did not match its checksum")
            }
            Self::Unsupported => {
                std::borrow::Cow::Borrowed("no video codec is published for this platform")
            }
        }
    }
}

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

/// What the decoder situation is right now, without touching the network.
///
/// Re-hashes the installed file against `TARGETS` at every call. A file at `installed_path`
/// persists across runs and can have been swapped, corrupted, or left from another platform, and
/// `from_blob_path`'s own table accepts a wrong-architecture build (see `TARGETS`).
pub(crate) fn status() -> CodecStatus {
    if !crate::state::boot_settings().video_codec_enabled {
        return CodecStatus::Disabled;
    }
    let Some(target) = current_target() else {
        return CodecStatus::Unsupported;
    };
    verify_installed(&installed_path(target), target)
}

/// Hashes `path` against `target.sha256`, apart from `status()` for tests to drive on a temp
/// file. A mismatch removes the file, for the next `ensure_available` to download afresh; only
/// bytes actually hashed can answer `Ready`.
fn verify_installed(path: &Path, target: &CodecTarget) -> CodecStatus {
    let Ok(data) = std::fs::read(path) else {
        return CodecStatus::NotDownloaded;
    };
    if sha256_bytes(&data) == target.sha256 {
        return CodecStatus::Ready(path.to_path_buf());
    }
    match std::fs::remove_file(path) {
        Ok(()) => crate::verr!("[VIDEO]   Codec digest mismatch, removed the installed file"),
        Err(e) => {
            crate::verr!(
                "[VIDEO]   Codec digest mismatch, and the bad file could not be removed: {e}"
            )
        }
    }
    CodecStatus::DigestMismatch
}

/// Provisioning happens once, however many cover jobs ask for it at once. Without it each one
/// downloads, inflates, hashes and renames onto the same path: the digest gate before the atomic
/// rename keeps the bytes identical, but the work and the request to Cisco are duplicated.
static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// [`status`] asked from async code, on the blocking pool.
///
/// The check hashes the whole installed library at every call, deliberately (see `status`):
/// inline, it would hold a runtime worker every other task shares, which `fetch_and_install`
/// already refuses for its own hash.
async fn status_off_runtime() -> CodecStatus {
    match tokio::task::spawn_blocking(status).await {
        Ok(status) => status,
        // A panic in `status` still unwinds the caller.
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // Nothing aborts this handle, which leaves the runtime shutting down as the only way
        // here: the answer the install below gives when its own blocking task ends that way.
        Err(e) => CodecStatus::DownloadFailed(e.to_string()),
    }
}

/// Same question, but allowed to fetch. Idempotent: an installed library short-circuits.
pub(crate) async fn ensure_available() -> CodecStatus {
    match status_off_runtime().await {
        CodecStatus::NotDownloaded => {}
        other => return other,
    }
    // `current_target` already answered `Some` inside `status`, or we would not be here.
    let Some(target) = current_target() else {
        return CodecStatus::Unsupported;
    };

    let _installing = INSTALL.lock().await;
    // Asked again after the wait: whoever held the lock may have installed the library meanwhile.
    // Checking only before the wait is what turns one installation into N.
    match status_off_runtime().await {
        CodecStatus::NotDownloaded => {}
        other => return other,
    }
    match fetch_and_install(target).await {
        Ok(path) => {
            crate::vprintln!("[VIDEO]   Codec installed ({})", path.display());
            CodecStatus::Ready(path)
        }
        Err(e) => {
            let status = install_failure(&e);
            crate::verr!("[VIDEO]   Codec install failed: {}", status.reason());
            status
        }
    }
}

/// What a failed install tells the user. Sync and IO-free, unlike `ensure_available`, so the
/// classification is testable on its own (see `CodecDigestMismatch` for why it reads the chain).
fn install_failure(e: &anyhow::Error) -> CodecStatus {
    if e.chain().any(|c| c.is::<CodecDigestMismatch>()) {
        CodecStatus::DigestMismatch
    } else {
        CodecStatus::DownloadFailed(e.to_string())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/video/codec.rs"]
mod tests;
