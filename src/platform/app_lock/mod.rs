//! Single-instance guard plus a "focus the running window" signal across launches.
//!
//! The first instance takes an OS lock and listens; a later launch detects it,
//! signals the running instance to raise its window, then exits before any shared
//! state (DB, the SDK credential LevelDB, Connect sockets, CEF) is touched. One
//! that finds the instance closing waits for it to leave and takes over. Windows
//! uses a named mutex + auto-reset event in a private namespace bounded by the
//! user's SID; Linux an flock on a file in the data directory, with an AF_UNIX
//! datagram socket in a private directory as the signal.

// Reaches only the focus-signal path, which exists on the platforms that have a
// guard to signal from.
#[cfg(any(windows, target_os = "linux"))]
use cef::*;

/// Raise the main window from any thread (posts to the CEF UI thread). No-op
/// until CEF is up.
#[cfg(any(windows, target_os = "linux"))]
fn post_focus() {
    if !crate::app_state::context_ready() {
        return;
    }
    let mut task = FocusWindowTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

#[cfg(any(windows, target_os = "linux"))]
wrap_task! {
    struct FocusWindowTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            crate::ui::app_window::AppWindow::raise_current();
        }
    }
}

/// What became of a deep link a second launch tried to hand over.
///
/// One named value rather than a bare bool, because the site that reports has to
/// tell "no link was carried" from "a link was carried and did not land", and to
/// say which of the causes it was. Collapsing them is what left a lost click with
/// no trace at all and one message standing for two different worlds.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug, PartialEq)]
enum HandOff {
    /// The running instance has the bytes.
    Landed,
    /// Longer than the channel carries, refused before anything was sent.
    TooLong,
    /// Nothing answered, and nothing appeared within the budget.
    NoListener,
    /// Something answered and stayed busy for the whole budget. Windows only: a
    /// datagram socket has no busy state to observe.
    #[cfg(windows)]
    Busy,
    /// The name answered, but another account holds it. Nothing was granted or
    /// written.
    #[cfg(windows)]
    ForeignServer,
    /// The name answered, but its owner could not be read, leaving nobody to vouch
    /// for the server.
    #[cfg(windows)]
    UnattributedServer,
    /// The running instance turned the channel away on purpose: it is on its way
    /// out and still holds the lock until it is gone. Linux only: a pipe has no
    /// way to say so, and its absence is all a client on Windows finds.
    #[cfg(target_os = "linux")]
    Closing,
    /// The attempt itself broke: a write that failed, or a channel we could not
    /// even open.
    Failed,
}

#[cfg(any(windows, target_os = "linux"))]
impl HandOff {
    /// For the one message that names why a click went nowhere.
    fn reason(&self) -> &'static str {
        match self {
            Self::Landed => "delivered",
            Self::TooLong => "too long for the channel",
            Self::NoListener => "nothing was listening",
            #[cfg(windows)]
            Self::Busy => "the listener stayed busy",
            #[cfg(windows)]
            Self::ForeignServer => "another account holds the channel",
            #[cfg(windows)]
            Self::UnattributedServer => "the channel's owner could not be read",
            #[cfg(target_os = "linux")]
            Self::Closing => "the running instance is closing",
            Self::Failed => "the channel failed",
        }
    }
}

/// What a launch asked of the running window besides its link. `Asked` says the
/// signal went, not that anything raised the window. The listener at the far end
/// may not exist.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug, PartialEq, Clone, Copy)]
enum Focus {
    Asked,
    /// Nothing was sent, or the send failed.
    #[cfg(target_os = "linux")]
    NotAsked,
    /// No focus event answers to the name, with the error that said so.
    #[cfg(windows)]
    Gone(u32),
    /// The event opened and signalling it failed, with the error.
    #[cfg(windows)]
    Unset(u32),
}

#[cfg(any(windows, target_os = "linux"))]
impl std::fmt::Display for Focus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Asked => f.write_str("focus asked"),
            #[cfg(target_os = "linux")]
            Self::NotAsked => f.write_str("the window was not asked to come forward"),
            #[cfg(windows)]
            Self::Gone(gle) => write!(f, "the focus event is gone, nothing was asked (gle={gle})"),
            #[cfg(windows)]
            Self::Unset(gle) => write!(f, "the focus signal was not set (gle={gle})"),
        }
    }
}

/// How a launch's link fared, and the focus request tried when it did not land.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug)]
struct Attempt {
    link: HandOff,
    focus: Focus,
}

#[cfg(any(windows, target_os = "linux"))]
impl Attempt {
    #[cfg(target_os = "linux")]
    fn unasked(link: HandOff) -> Self {
        Self {
            link,
            focus: Focus::NotAsked,
        }
    }

    /// Ungated: this process exits before reading the persisted log level, and a
    /// click whose link never opens leaves no other trace.
    fn report_lost(&self) {
        crate::verr!(
            "[LOCK]   deep link not delivered ({}); {}",
            self.link.reason(),
            self.focus
        );
    }
}

/// How long a second launch keeps trying before it gives the launch up: to reach
/// the channel, and to outwait a running instance that is leaving.
///
/// One deadline for both, started when the lock is found taken. The tightest
/// ceiling on a protocol handler is `xdg-open`'s 5 s portal timeout; Firefox
/// retries its profile lock for 5 s and Chromium on Windows its lockfile for 5 s,
/// for the same instance still closing down.
#[cfg(any(windows, target_os = "linux"))]
const HANDOFF_BUDGET_MS: u64 = 5000;

/// How long a launch looks for a channel that is not there, from a holder that is
/// not leaving, before reporting it. A Linux holder binds right after taking the
/// lock (45 ms at worst over 3000 trials with twice as many busy threads as cores);
/// an absence past that is no race.
#[cfg(any(windows, target_os = "linux"))]
const NO_LISTENER_GRACE_MS: u64 = 250;

/// How long closing waits for a listener to stop.
#[cfg(any(windows, target_os = "linux"))]
const CLOSE_WAIT_MS: u64 = 500;

/// Waits for a listener thread, but no longer than `budget`: a listener held by a
/// peer that never finishes writing must not hold the quit behind it.
#[cfg(any(windows, target_os = "linux"))]
fn join_within(listener: std::thread::JoinHandle<()>, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while !listener.is_finished() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let _ = listener.join();
    true
}

/// Turns the channel away for the rest of the process: a link or focus request sent after
/// this fails visibly, and its sender waits for the lock. Called where the message loop is
/// told to quit and where a launch ends after taking the lock (it outlives the service by
/// the whole teardown). Idempotent; a no-op before the lock is taken and on macOS.
pub(crate) fn close_channel() {
    #[cfg(windows)]
    windows_impl::close_channel();
    #[cfg(target_os = "linux")]
    linux_impl::close_channel();
}

/// Whose kernel object this is, the link pipe or the legacy lock, by the owner the
/// kernel stamped on it. A name is a first-come claim any account can make; the
/// owner is the one fact its creator cannot forge.
#[cfg(windows)]
#[derive(Debug, PartialEq)]
enum ObjectOwner {
    Ours,
    /// Another account holds the name, kept for the log line.
    Foreign(String),
    /// The owner could not be read.
    Unattributed,
}

/// The owners our own token stamps on what it creates: its user, and its default
/// owner, which is BUILTIN\Administrators when the token is elevated.
#[cfg(windows)]
struct OwnIdentity {
    user: String,
    default_owner: String,
}

/// Exact equality on both sides' `ConvertSidToStringSidW` form: a SID that merely
/// starts like ours is another account.
#[cfg(windows)]
fn classify_owner(owner: Option<&str>, own: &OwnIdentity) -> ObjectOwner {
    match owner {
        Some(sid) if sid == own.user || sid == own.default_owner => ObjectOwner::Ours,
        Some(sid) => ObjectOwner::Foreign(sid.to_owned()),
        None => ObjectOwner::Unattributed,
    }
}

/// What became of the pipe a clicked link travels on, for the instance that owns
/// the lock.
///
/// Windows only. Linux has one socket for focus and links, whose absence is an
/// `Unserved` cause, and macOS never launches a second process at all.
#[cfg(windows)]
enum LinkChannel {
    /// A listener holds the address; a link arriving now reaches the window.
    Listening,
    /// Creating it or its listener failed, and nothing else will ever say why
    /// clicking a link stopped working.
    Unavailable,
    /// Never attempted, the guard having stood down for the `Unserved::Unguarded`
    /// cause the lock carries.
    NotAttempted,
}

/// What the first instance went without, held until `report_channels` can say it.
/// A data channel no thread reads is closed; a marker another process reads stays.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug)]
enum Unserved {
    /// With no lock taken there is no channel either, and no second launch is held off.
    Unguarded(Unguarded),
    /// The lock holds, but no private directory could name a channel.
    #[cfg(target_os = "linux")]
    NoChannelDir,
    /// The lock holds, but the channel's socket could not be bound.
    #[cfg(target_os = "linux")]
    Bind(std::io::Error),
    #[cfg(windows)]
    Focus(std::io::Error),
    /// The event only earlier builds signal.
    #[cfg(windows)]
    LegacyFocus(std::io::Error),
    #[cfg(windows)]
    Link(std::io::Error),
    /// The event that stops the focus listeners, which closing then cannot reach.
    #[cfg(windows)]
    Stop(std::io::Error),
    /// One socket carries both focus and links here.
    #[cfg(target_os = "linux")]
    Channel(std::io::Error),
    /// The name earlier builds lock on stays bound; they still exit on it.
    #[cfg(target_os = "linux")]
    Legacy(std::io::Error),
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug)]
enum Unguarded {
    #[cfg(target_os = "linux")]
    DataDir(std::io::Error),
    #[cfg(target_os = "linux")]
    DataDirRefused(linux_impl::DirRefusal),
    #[cfg(target_os = "linux")]
    LockFile(std::io::Error),
    #[cfg(target_os = "linux")]
    Lock(std::io::Error),
    #[cfg(windows)]
    Identity,
    #[cfg(windows)]
    Namespace(u32),
    #[cfg(windows)]
    Mutex(u32),
}

#[cfg(any(windows, target_os = "linux"))]
impl std::fmt::Display for Unguarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(target_os = "linux")]
            Self::DataDir(e) => write!(f, "the data directory cannot be created ({e})"),
            #[cfg(target_os = "linux")]
            Self::DataDirRefused(refusal) => {
                write!(f, "the data directory is refused ({refusal:?})")
            }
            #[cfg(target_os = "linux")]
            Self::LockFile(e) => write!(f, "the lock file cannot be opened ({e})"),
            #[cfg(target_os = "linux")]
            Self::Lock(e) => write!(f, "the lock file cannot be locked ({e})"),
            #[cfg(windows)]
            Self::Identity => f.write_str("SID lookup failed"),
            #[cfg(windows)]
            Self::Namespace(gle) => write!(f, "private namespace unavailable (gle={gle})"),
            #[cfg(windows)]
            Self::Mutex(gle) => write!(f, "CreateMutexW failed (gle={gle})"),
        }
    }
}

/// Held for the process lifetime; dropping it releases the lock.
pub(crate) struct AppLock {
    #[cfg(windows)]
    _mutex: windows_impl::MutexGuard,
    #[cfg(windows)]
    link: LinkChannel,
    #[cfg(windows)]
    closer: Option<std::sync::Arc<windows_impl::Closer>>,
    #[cfg(target_os = "linux")]
    _guard: Option<linux_impl::Guard>,
    #[cfg(any(windows, target_os = "linux"))]
    unserved: Vec<Unserved>,
    #[cfg(not(any(windows, target_os = "linux")))]
    _unsupported: (),
}

impl AppLock {
    /// Say what the first instance went without, once the session log is ours to write.
    ///
    /// The verdict waits on the type instead of leaving as a `verr!` where it is
    /// found. `verr!` opens the log file itself, and finding that file already
    /// open is what makes `adopt_session_log` stand its rotation down.
    pub(crate) fn report_channels(&self) {
        #[cfg(windows)]
        if matches!(self.link, LinkChannel::Unavailable) {
            crate::verr!("[LOCK]   no link pipe; deep links cannot reach this window");
        }
        #[cfg(target_os = "linux")]
        if let Some(guard) = &self._guard {
            guard.report_deaths();
        }
        #[cfg(any(windows, target_os = "linux"))]
        for unserved in &self.unserved {
            match unserved {
                #[cfg(windows)]
                Unserved::Focus(e) => crate::verr!(
                    "[LOCK]   no focus listener ({e}); a second launch cannot raise this window"
                ),
                #[cfg(windows)]
                Unserved::LegacyFocus(e) => crate::verr!(
                    "[LOCK]   no legacy focus listener ({e}); earlier builds cannot raise this window"
                ),
                #[cfg(windows)]
                Unserved::Link(e) => {
                    crate::verr!("[LOCK]   the link pipe had no thread to serve it ({e})")
                }
                Unserved::Unguarded(cause) => {
                    crate::verr!("[LOCK]   {cause}; single-instance guard disabled")
                }
                #[cfg(windows)]
                Unserved::Stop(e) => crate::verr!(
                    "[LOCK]   no stop event ({e}); the focus listeners cannot be closed"
                ),
                #[cfg(target_os = "linux")]
                Unserved::NoChannelDir => crate::verr!(
                    "[LOCK]   no private directory for the channel; focus and links cannot arrive"
                ),
                #[cfg(target_os = "linux")]
                Unserved::Bind(e) => crate::verr!(
                    "[LOCK]   the channel cannot be bound ({e}); focus and links cannot arrive"
                ),
                #[cfg(target_os = "linux")]
                Unserved::Channel(e) => crate::verr!(
                    "[LOCK]   no channel listener ({e}); focus and links cannot arrive"
                ),
                #[cfg(target_os = "linux")]
                Unserved::Legacy(e) => crate::verr!(
                    "[LOCK]   no legacy listener ({e}); earlier builds cannot reach this instance"
                ),
            }
        }
    }
}

/// `Some` if this is the first instance (keep the guard alive); `None` if another
/// instance is already running (it has been handed `url`, or a bare focus request
/// when there is none; exit).
pub(crate) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
    #[cfg(windows)]
    {
        windows_impl::acquire_or_signal(url)
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::acquire_or_signal(url)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // macOS never launches a second process for a URL. LaunchServices
        // activates the running app and sends it an Apple Event instead.
        let _ = url;
        Some(AppLock { _unsupported: () })
    }
}

#[cfg(windows)]
mod windows_impl;

#[cfg(target_os = "linux")]
mod linux_impl;

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../tests/unit/platform/app_lock.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/platform/app_lock_server.rs"]
mod server_tests;
