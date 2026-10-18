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

/// A download whose digest does not match `TARGETS`, carried by `FetchError::Digest`.
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

/// Why an install failed, tagged by the STAGE that failed. No `From`: every `?` in
/// `fetch_and_install` names its stage; a new local step cannot fall by default into the class
/// that retries, the way an `io::Error` from a full disk would look like one from the network.
#[derive(Debug)]
enum FetchError {
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
    fn class(&self) -> Option<FailureClass> {
        Some(match self {
            Self::Unreachable(_) | Self::Body(_) => FailureClass::Network,
            Self::Status(s) if s.is_server_error() || matches!(s.as_u16(), 408 | 429) => {
                FailureClass::Network
            }
            Self::Status(_) | Self::OffHost | Self::NoTarget | Self::Redirected(_) => {
                FailureClass::Refused
            }
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
fn write_chain(
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

/// Why the codec failed, apart from whether to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The host could not be reached, or answered that it could not serve right now.
    Network,
    /// The host answered, and the answer was no.
    Refused,
    /// The bytes arrived and were not the codec.
    Corrupt,
    /// Every local fault (a full disk, a permission, a file another program holds).
    Disk,
    /// The verified file would not load in this process.
    Unloadable,
    /// A supervisor step panicked: a bug of ours, whose cause says nothing of whether it recurs.
    Crashed,
}

impl FailureClass {
    /// The only failure text a class sends out of the process, a constant: no path or OS
    /// message travels with it to the page.
    fn phrase(self) -> &'static str {
        match self {
            Self::Network => "Cisco's server could not be reached",
            Self::Refused => "Cisco's server did not provide the video codec",
            Self::Corrupt => "the downloaded video codec failed its integrity check",
            Self::Disk => {
                "the video codec file could not be read or written: another program may hold it, or the disk refuses it"
            }
            Self::Unloadable => "the video codec could not be loaded on this system",
            Self::Crashed => "the video codec service hit an internal error",
        }
    }
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
async fn fetch_and_install(target: &'static CodecTarget) -> Result<PathBuf, FetchError> {
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
enum Verified {
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
fn verify_installed(path: &Path, target: &CodecTarget) -> Verified {
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

/// `f` on the blocking pool, where a hash or a `dlopen` does not hold a runtime worker every other
/// task shares. `None` when the runtime cancelled it: nothing aborts these handles, which leaves
/// a shutdown as the only way there. A panic still unwinds the caller, up to `Core::step`.
async fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Some(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => None,
    }
}

/// Where the decoder stands, as last settled. The supervisor's task is its only writer: the
/// `watch::Sender` lives in that task alone, and nothing else in the process can publish.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Nothing has been proven yet.
    Starting,
    Downloading,
    Ready(PathBuf),
    Unavailable(Unavailable),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Off,
    Unsupported,
    Failed(FailureClass),
}

/// Why a cover gets no decoder: what the supervisor published, or a supervisor that is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoDecoder {
    Unavailable(Unavailable),
    /// Never started, or its task ended with the runtime: a step that panics fails `Crashed`.
    Stopped,
}

impl NoDecoder {
    /// A short phrase for the UI, not a log line. `'static`: no cause can be formatted into it.
    pub(crate) fn phrase(self) -> &'static str {
        match self {
            Self::Unavailable(Unavailable::Off) => "video decoding is turned off in settings",
            Self::Unavailable(Unavailable::Unsupported) => {
                "no video codec is published for this platform"
            }
            Self::Unavailable(Unavailable::Failed(class)) => class.phrase(),
            Self::Stopped => "the video codec service has stopped",
        }
    }
}

/// An ask answered from the phase. `Starting` and `Downloading` only outlive a step once the
/// supervisor is gone: a live one answers after it settled.
fn answer(phase: &Phase) -> Result<PathBuf, NoDecoder> {
    match phase {
        Phase::Ready(path) => Ok(path.clone()),
        Phase::Unavailable(u) => Err(NoDecoder::Unavailable(*u)),
        Phase::Starting | Phase::Downloading => Err(NoDecoder::Stopped),
    }
}

/// What the supervisor publishes. `seq` moves with every change; the page keeps the newest of
/// a push and a pull that crossed: they travel different paths.
#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    seq: u64,
    /// The user's switch, as the backend holds it.
    enabled: bool,
    phase: Phase,
}

fn publish(tx: &tokio::sync::watch::Sender<Snapshot>, phase: Phase, enabled: bool) {
    tx.send_if_modified(|current| {
        if current.phase == phase && current.enabled == enabled {
            return false;
        }
        current.phase = phase;
        current.enabled = enabled;
        current.seq += 1;
        true
    });
}

/// How a failure gets asked again, as the settings row tells it. A named enum rather than a
/// `retrying` bool: "on the next demand" and "never" want different words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Recovery {
    Timer,
    OnDemand,
    Never,
}

impl FailureClass {
    fn recovery(self) -> Recovery {
        match self {
            Self::Network => Recovery::Timer,
            Self::Refused | Self::Corrupt | Self::Disk | Self::Crashed => Recovery::OnDemand,
            Self::Unloadable => Recovery::Never,
        }
    }
}

/// The page's copy of a [`Snapshot`], with no path and only `'static` text: no cause leaves the
/// process this way either.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PhaseView {
    Starting,
    Off,
    Unsupported,
    Downloading,
    Ready,
    Failed {
        reason: &'static str,
        recovery: Recovery,
    },
}

#[derive(Debug, PartialEq, serde::Serialize)]
struct SnapshotView {
    seq: u64,
    enabled: bool,
    phase: PhaseView,
}

fn view(snapshot: &Snapshot) -> SnapshotView {
    let phase = match snapshot.phase {
        Phase::Starting => PhaseView::Starting,
        Phase::Downloading => PhaseView::Downloading,
        Phase::Ready(_) => PhaseView::Ready,
        Phase::Unavailable(Unavailable::Off) => PhaseView::Off,
        Phase::Unavailable(Unavailable::Unsupported) => PhaseView::Unsupported,
        Phase::Unavailable(Unavailable::Failed(class)) => PhaseView::Failed {
            reason: class.phrase(),
            recovery: class.recovery(),
        },
    };
    SnapshotView {
        seq: snapshot.seq,
        enabled: snapshot.enabled,
        phase,
    }
}

/// The event the page listens to, and the request it pulls with at subscription.
const STATE_CHANNEL: &str = "settings.video_codec_state";

/// A change published before the page listens is lost here; the page's pull at subscription
/// makes up for it.
async fn emit_changes(mut state: tokio::sync::watch::Receiver<Snapshot>) {
    while state.changed().await.is_ok() {
        let view = view(&state.borrow_and_update());
        crate::app_state::emit_ipc_event_with_data(STATE_CHANNEL, &view);
    }
}

const FIRST_RETRY: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Doubles up to `MAX_RETRY`, then holds: a machine offline for the day costs one request per
/// half hour.
fn next_retry_delay(delay: std::time::Duration) -> std::time::Duration {
    delay.saturating_mul(2).min(MAX_RETRY)
}

/// How often a demand may ask again after a `Network` failure. A fixed floor rather than the
/// backoff, since a demand proves TIDAL's network works yet says nothing of Cisco's host (a
/// firewall blocks it while TIDAL loads). It spaces the demands only: the timer keeps its own
/// backoff, whose first attempts come closer together than this.
const NETWORK_DEMAND_FLOOR: std::time::Duration = std::time::Duration::from_secs(5 * 60);

impl FailureClass {
    /// Only a lost network heals by itself; only it has a timer asking again.
    fn timer(self) -> bool {
        match self {
            Self::Network => true,
            Self::Refused | Self::Corrupt | Self::Disk | Self::Unloadable | Self::Crashed => false,
        }
    }

    /// How long after the last attempt a demand may attempt again, `wait` being the backoff that
    /// attempt earned. `None`: a fact about this process; only a relaunch or the user's switch
    /// changes it.
    fn demand_floor(self, wait: std::time::Duration) -> Option<std::time::Duration> {
        match self {
            Self::Network => Some(NETWORK_DEMAND_FLOOR),
            // An attempt may cost a download or a hash: it rides the backoff.
            Self::Refused | Self::Corrupt | Self::Disk | Self::Crashed => Some(wait),
            Self::Unloadable => None,
        }
    }
}

/// What woke the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Boot,
    Timer,
    /// A cover job asking, or a request the artwork filter armed for.
    Demand,
    /// A decode could not load the file this phase called ready.
    Recheck,
    /// The user moved the switch: the manual retry of any failure.
    Switch,
}

/// What a step does. A named enum: `plan` is a table the tests read whole.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Off,
    Unsupported,
    Verify,
    Hold,
}

/// `due`: the timer's own deadline has passed; a stale wake-up cannot beat a backoff a later
/// attempt extended. `since`: time since the last attempt, whoever made it. `wait`: the backoff
/// that attempt earned.
fn plan(
    phase: &Phase,
    wanted: bool,
    target_known: bool,
    why: Why,
    due: bool,
    since: Option<std::time::Duration>,
    wait: std::time::Duration,
) -> Plan {
    if !wanted {
        return Plan::Off;
    }
    if !target_known {
        return Plan::Unsupported;
    }
    match phase {
        Phase::Starting | Phase::Unavailable(Unavailable::Off) => Plan::Verify,
        Phase::Downloading | Phase::Unavailable(Unavailable::Unsupported) => Plan::Hold,
        Phase::Ready(_) => match why {
            Why::Recheck => Plan::Verify,
            Why::Boot | Why::Timer | Why::Demand | Why::Switch => Plan::Hold,
        },
        Phase::Unavailable(Unavailable::Failed(class)) => match why {
            // An off and an on the task read as one level still ask for the retry.
            Why::Switch => Plan::Verify,
            Why::Timer if class.timer() && due => Plan::Verify,
            Why::Demand
                if class
                    .demand_floor(wait)
                    .is_some_and(|floor| since.is_none_or(|d| d >= floor)) =>
            {
                Plan::Verify
            }
            Why::Boot | Why::Timer | Why::Demand | Why::Recheck => Plan::Hold,
        },
    }
}

/// Whether a failure is news. The same class again is the state the page already shows, and
/// `verr!` is budgeted; a different one is.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Loud,
    Quiet,
}

fn log_gate(last: Option<FailureClass>, new: FailureClass) -> Gate {
    if last == Some(new) {
        Gate::Quiet
    } else {
        Gate::Loud
    }
}

/// The supervisor's whole contact with the disk, the network and the loader, which the tests
/// script to run a day in paused time.
trait Io: Send + Sync + 'static {
    fn target_known(&self) -> bool;
    /// `None`: the runtime is shutting down.
    fn verify(&self) -> impl Future<Output = Option<Verified>> + Send;
    fn fetch(&self) -> impl Future<Output = Result<PathBuf, FetchError>> + Send;
    /// `None`: the runtime is shutting down.
    fn probe(&self, path: PathBuf) -> impl Future<Output = Option<anyhow::Result<()>>> + Send;
    fn report(&self, gate: Gate, err: anyhow::Error);
}

struct OnDisk {
    target: Option<&'static CodecTarget>,
}

impl Io for OnDisk {
    fn target_known(&self) -> bool {
        self.target.is_some()
    }

    async fn verify(&self) -> Option<Verified> {
        // `plan` never verifies without a target.
        let target = self.target?;
        off_runtime(move || verify_installed(&installed_path(target), target)).await
    }

    async fn fetch(&self) -> Result<PathBuf, FetchError> {
        // `plan` never fetches without a target; were it to, there is no build to ask for.
        let target = self.target.ok_or(FetchError::NoTarget)?;
        let path = fetch_and_install(target).await?;
        crate::vprintln!("[VIDEO]   Codec installed ({})", path.display());
        Ok(path)
    }

    /// Each decode loads its own copy (`Decoder` takes the API by value); the probe's handle
    /// is dropped here.
    async fn probe(&self, path: PathBuf) -> Option<anyhow::Result<()>> {
        off_runtime(move || load_api(&path).map(drop)).await
    }

    fn report(&self, gate: Gate, err: anyhow::Error) {
        match gate {
            Gate::Loud => crate::verr!("[VIDEO]   {err:#}"),
            Gate::Quiet => crate::vprintln!("[VIDEO]   {err:#}"),
        }
    }
}

/// The supervisor's memory, which only its task touches.
struct Core<I: Io> {
    io: I,
    /// The backoff the last failure earned.
    wait: std::time::Duration,
    failures: u32,
    last_attempt: Option<tokio::time::Instant>,
    /// When the timer asks again; `None` for a class without one.
    next_at: Option<tokio::time::Instant>,
    last_failure: Option<FailureClass>,
    /// Whether the verified library loads in this process, asked once until a switch click clears
    /// it. The digest pins the file; what can still refuse it is the environment (a loader
    /// dependency the system lacks, a glibc too old).
    loads: Option<bool>,
}

impl<I: Io> Core<I> {
    fn new(io: I) -> Self {
        Self {
            io,
            wait: FIRST_RETRY,
            failures: 0,
            last_attempt: None,
            next_at: None,
            last_failure: None,
            loads: None,
        }
    }

    /// One step, a panic anywhere in it ending as a published `Crashed` (the blocking closures
    /// re-raise theirs through `off_runtime`). The task keeps serving: dead, it would leave the
    /// last phase published for the rest of the process, a `Ready` whose file is gone included.
    async fn step(&mut self, tx: &tokio::sync::watch::Sender<Snapshot>, wanted: bool, why: Why) {
        use futures_util::FutureExt;
        if let Err(payload) = std::panic::AssertUnwindSafe(self.run_step(tx, wanted, why))
            .catch_unwind()
            .await
        {
            self.crashed(tx, wanted, payload);
        }
    }

    async fn run_step(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
        why: Why,
    ) {
        let phase = tx.borrow().phase.clone();
        let since = self.last_attempt.map(|t| t.elapsed());
        let due = self
            .next_at
            .is_some_and(|t| tokio::time::Instant::now() >= t);
        match plan(
            &phase,
            wanted,
            self.io.target_known(),
            why,
            due,
            since,
            self.wait,
        ) {
            Plan::Hold => {}
            Plan::Off => {
                self.next_at = None;
                publish(tx, Phase::Unavailable(Unavailable::Off), wanted);
            }
            Plan::Unsupported => {
                publish(tx, Phase::Unavailable(Unavailable::Unsupported), wanted);
            }
            Plan::Verify => {
                if why == Why::Switch {
                    // The user's click is the manual retry: nothing remembered holds it back,
                    // not even a library that would not load (a system library may have changed).
                    self.failures = 0;
                    self.wait = FIRST_RETRY;
                    self.last_attempt = None;
                    self.next_at = None;
                    self.loads = None;
                }
                if let Some(next) = self.settle(tx, wanted).await {
                    publish(tx, next, wanted);
                }
            }
        }
    }

    /// Verify, fetch if needed, probe once. `None` when the runtime shut down underneath: nothing
    /// was decided or published.
    async fn settle(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
    ) -> Option<Phase> {
        self.last_attempt = Some(tokio::time::Instant::now());
        let path = match self.io.verify().await? {
            Verified::Installed(path) => path,
            Verified::Stuck(e) => {
                let err = anyhow::Error::from(e)
                    .context("the installed codec is unusable and cannot be replaced");
                return Some(self.failed(FailureClass::Disk, err));
            }
            // Both leave nothing at the path, which is what the fetch downloads onto.
            Verified::Absent | Verified::Removed => {
                publish(tx, Phase::Downloading, wanted);
                match self.io.fetch().await {
                    Ok(path) => path,
                    Err(e) => {
                        let class = e.class()?;
                        let err = anyhow::Error::from(e).context("codec install failed");
                        return Some(self.failed(class, err));
                    }
                }
            }
        };
        // A `Ready` the digest alone vouches for is loaded once before it is believed: a library
        // that hashes right but cannot load would otherwise disarm the artwork filter for covers
        // no decode can paint.
        let loads = match self.loads {
            Some(loads) => loads,
            None => {
                let loads = match self.io.probe(path.clone()).await? {
                    Ok(()) => true,
                    Err(e) => {
                        let err = e.context("the codec does not load on this system");
                        let gate = log_gate(self.last_failure, FailureClass::Unloadable);
                        self.io.report(gate, err);
                        false
                    }
                };
                self.loads = Some(loads);
                loads
            }
        };
        if !loads {
            self.last_failure = Some(FailureClass::Unloadable);
            self.next_at = None;
            return Some(Phase::Unavailable(Unavailable::Failed(
                FailureClass::Unloadable,
            )));
        }
        self.last_failure = None;
        self.failures = 0;
        self.wait = FIRST_RETRY;
        self.next_at = None;
        Some(Phase::Ready(path))
    }

    /// Published before anything is logged, since the logger may be what panicked, and the log
    /// is guarded itself: a second panic there must not end the task the first one spared.
    fn crashed(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
        payload: Box<dyn std::any::Any + Send>,
    ) {
        // A panic before `settle` stamped it would leave `None`, which `plan` reads as a floor
        // already passed.
        self.last_attempt = Some(tokio::time::Instant::now());
        let gate = log_gate(self.last_failure, FailureClass::Crashed);
        let phase = self.record_failure(FailureClass::Crashed);
        publish(tx, phase, wanted);
        let cause = crate::connect::runtime::panic_payload_to_string(payload);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.io.report(
                gate,
                anyhow::anyhow!("a codec supervisor step panicked: {cause}"),
            );
        }));
    }

    fn failed(&mut self, class: FailureClass, err: anyhow::Error) -> Phase {
        self.io.report(log_gate(self.last_failure, class), err);
        self.record_failure(class)
    }

    fn record_failure(&mut self, class: FailureClass) -> Phase {
        self.last_failure = Some(class);
        self.wait = if self.failures == 0 {
            FIRST_RETRY
        } else {
            next_retry_delay(self.wait)
        };
        self.failures += 1;
        self.next_at = class
            .timer()
            .then(|| tokio::time::Instant::now() + self.wait);
        Phase::Unavailable(Unavailable::Failed(class))
    }
}

enum Wake {
    Ask(tokio::sync::oneshot::Sender<Result<PathBuf, NoDecoder>>),
    Recheck,
    /// The switch moved. Carries nothing: the task reads `wanted` as a level; a burst of
    /// toggles settles once, on the last position.
    Switch,
}

/// The task's end of the wake channel. A switch is a level, and the ones queued back to back are
/// one retry (each would otherwise plan its own `Verify` and fetch a failure once per click).
/// Only a run is folded; a switch behind an ask or a recheck keeps its place.
struct Wakes {
    rx: tokio::sync::mpsc::UnboundedReceiver<Wake>,
    /// The first message after a run of switches, already taken off the channel.
    held: Option<Wake>,
}

impl Wakes {
    async fn recv(&mut self) -> Option<Wake> {
        match self.held.take() {
            Some(wake) => Some(wake),
            None => self.rx.recv().await,
        }
    }

    /// Drops the switches queued right behind the one just taken.
    fn fold_switches(&mut self) {
        while let Ok(wake) = self.rx.try_recv() {
            if !matches!(wake, Wake::Switch) {
                self.held = Some(wake);
                break;
            }
        }
    }
}

/// The handle every caller holds. It carries a `Receiver` and the ways to wake the task, never the
/// `Sender`. Its entries are plain `send`, `store` and `notify_one`, callable from CEF's IO
/// thread, the UI thread or a blocking task with no runtime context.
struct Supervisor {
    state: tokio::sync::watch::Receiver<Snapshot>,
    wakes: tokio::sync::mpsc::UnboundedSender<Wake>,
    demand: std::sync::Arc<tokio::sync::Notify>,
    wanted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Supervisor {
    fn start<I: Io>(rt: &tokio::runtime::Handle, io: I, wanted: bool) -> Self {
        let (tx, state) = tokio::sync::watch::channel(Snapshot {
            seq: 0,
            enabled: wanted,
            phase: Phase::Starting,
        });
        let (wakes, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Wakes { rx, held: None };
        let demand = std::sync::Arc::new(tokio::sync::Notify::new());
        let wanted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(wanted));
        rt.spawn(supervise(
            Core::new(io),
            tx,
            rx,
            demand.clone(),
            wanted.clone(),
        ));
        Self {
            state,
            wakes,
            demand,
            wanted,
        }
    }

    /// A download under way finishes first: the task is busy with it, and `Off` follows.
    fn set_enabled(&self, on: bool) {
        self.wanted.store(on, std::sync::atomic::Ordering::Release);
        let _ = self.wakes.send(Wake::Switch);
    }

    fn is_ready(&self) -> bool {
        matches!(self.state.borrow().phase, Phase::Ready(_))
    }

    /// Asks share the task's queue: fifty cover jobs at once are one fetch and fifty answers.
    async fn ask(&self) -> Result<PathBuf, NoDecoder> {
        if let Phase::Ready(path) = &self.state.borrow().phase {
            return Ok(path.clone());
        }
        let (reply, answered) = tokio::sync::oneshot::channel();
        if self.wakes.send(Wake::Ask(reply)).is_err() {
            return answer(&self.state.borrow().phase);
        }
        answered
            .await
            .unwrap_or_else(|_| answer(&self.state.borrow().phase))
    }

    /// `notify_one` stores a single permit; a burst of requests wakes the task once. Nothing to
    /// wake for unless something failed.
    fn demand(&self) {
        if matches!(
            self.state.borrow().phase,
            Phase::Unavailable(Unavailable::Failed(_))
        ) {
            self.demand.notify_one();
        }
    }

    fn recheck(&self) {
        // A task already gone has nothing to recheck.
        let _ = self.wakes.send(Wake::Recheck);
    }
}

async fn supervise<I: Io>(
    mut core: Core<I>,
    tx: tokio::sync::watch::Sender<Snapshot>,
    mut wakes: Wakes,
    demand: std::sync::Arc<tokio::sync::Notify>,
    wanted: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let wanted = || wanted.load(std::sync::atomic::Ordering::Acquire);
    core.step(&tx, wanted(), Why::Boot).await;
    loop {
        let next_at = core.next_at;
        let timer = async move {
            match next_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            wake = wakes.recv() => match wake {
                None => return,
                Some(Wake::Ask(reply)) => {
                    core.step(&tx, wanted(), Why::Demand).await;
                    let _ = reply.send(answer(&tx.borrow().phase));
                }
                Some(Wake::Recheck) => core.step(&tx, wanted(), Why::Recheck).await,
                Some(Wake::Switch) => {
                    wakes.fold_switches();
                    core.step(&tx, wanted(), Why::Switch).await;
                }
            },
            () = demand.notified() => core.step(&tx, wanted(), Why::Demand).await,
            () = timer => core.step(&tx, wanted(), Why::Timer).await,
        }
    }
}

static SUPERVISOR: std::sync::OnceLock<Supervisor> = std::sync::OnceLock::new();

/// Starts the one supervisor, whatever the switch says: with it off, it answers `Off` and fetches
/// nothing. Fetched at launch, as Firefox does, rather than on the first animated cover: until the
/// library is proven the artwork filter keeps the now-playing view on still covers. Waiting for
/// a cover to ask would leave it still until an album page happened to.
pub(crate) fn start(rt: &tokio::runtime::Handle, enabled: bool) {
    SUPERVISOR.get_or_init(|| {
        let io = OnDisk {
            target: current_target(),
        };
        let supervisor = Supervisor::start(rt, io, enabled);
        rt.spawn(emit_changes(supervisor.state.clone()));
        supervisor
    });
}

/// The page's pull, answered from the published snapshot: a `borrow` and serde, no IO; it
/// does not block the UI thread it runs on. `null` before [`start`], which the page ignores.
pub(crate) fn state_json() -> String {
    SUPERVISOR
        .get()
        .and_then(|supervisor| serde_json::to_string(&view(&supervisor.state.borrow())).ok())
        .unwrap_or_else(|| "null".to_string())
}

/// The artwork filter reads it once per request. False until the library is proven (TIDAL
/// then serves the still cover), and never on a platform or network where it cannot be.
pub(crate) fn is_ready() -> bool {
    SUPERVISOR.get().is_some_and(Supervisor::is_ready)
}

/// The decoder's path for a cover job, fetching it first when the schedule allows.
pub(crate) async fn decoder() -> Result<PathBuf, NoDecoder> {
    match SUPERVISOR.get() {
        Some(supervisor) => supervisor.ask().await,
        None => Err(NoDecoder::Stopped),
    }
}

/// A request the artwork filter armed for: someone wants animated covers. Free unless the codec
/// failed, and then rate-limited by the class's floor.
pub(crate) fn note_demand() {
    if let Some(supervisor) = SUPERVISOR.get() {
        supervisor.demand();
    }
}

/// The user's switch, live. Off keeps the installed file (Firefox removes nothing either); on
/// again only verifies it.
pub(crate) fn set_enabled(on: bool) {
    if let Some(supervisor) = SUPERVISOR.get() {
        supervisor.set_enabled(on);
    }
}

/// A decode could not load the file the supervisor called ready: verify it again.
pub(crate) fn recheck() {
    if let Some(supervisor) = SUPERVISOR.get() {
        supervisor.recheck();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/video/codec.rs"]
mod tests;
