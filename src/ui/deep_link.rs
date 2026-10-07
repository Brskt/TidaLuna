//! Turn an OS-delivered `tidal://` URL into a navigation target, or refuse it.
//!
//! Registering the scheme with the OS makes this an ingress any process on the
//! machine can drive. The incoming string is therefore never trusted to name a
//! destination; only its path and query survive, grafted onto our own origin.
//! A URL that still fails to parse back onto that host is dropped rather than
//! repaired.
//!
//! Only TIDAL's content routes are reachable. The alternative, naming the
//! sensitive routes and refusing those, fails in the wrong direction. A route
//! TIDAL adds that we never listed would be open until someone noticed, and
//! nobody notices a callback that worked. Four of its routes redeem a credential
//! or mutate state from their query string alone, and `/login` was one.

use std::ffi::OsString;
use std::sync::Mutex;

use cef::*;
use url::Url;

use crate::ui::nav::HOST_DESKTOP;

/// Registration and routing read this same constant; a packaging channel that
/// hardcodes the scheme instead drifts from it.
pub(crate) const SCHEME: &str = "tidal";

/// Case-insensitive, because KDE and the Windows shell both preserve whatever
/// case the emitter used and neither normalises it for us.
fn strip_scheme(raw: &str) -> Option<&str> {
    let (head, rest) = raw.split_at_checked(SCHEME.len() + "://".len())?;
    let scheme = head.strip_suffix("://")?;
    scheme.eq_ignore_ascii_case(SCHEME).then_some(rest)
}

/// A `tidal://` URL that passed both checks, holding the spelling that was
/// checked rather than the one that arrived.
///
/// Two navigations read it, the startup load and the script that routes an
/// already-loaded page, and each used to derive its own string from the parsed
/// URL. Deriving twice is what let the checked first segment and the delivered
/// one disagree; the value is settled once here and only read afterwards.
pub(crate) struct DeepLinkTarget(Url);

impl DeepLinkTarget {
    /// The path, query and fragment, for routing the page already loaded.
    ///
    /// Starts with exactly one slash. The constructor refuses an empty first
    /// segment, which is the only way a second slash reaches the front, and a
    /// browser resolving this against our origin reads a leading `//` as an
    /// authority (`//album/1` names the host `album`, not a path of ours).
    pub(crate) fn route(&self) -> &str {
        &self.0[url::Position::BeforePath..]
    }
}

impl std::fmt::Display for DeepLinkTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The form a target takes in the log. `Display` is the whole URL, right to act on
/// and wrong to write down, since the query carries whatever the sender typed and
/// the path past the first segment is unchecked and unbounded. The route survives
/// both cuts, and the route is what the log is read for.
fn for_log(target: &DeepLinkTarget) -> String {
    let full = target.to_string();
    crate::util::truncate_str(&crate::util::redact_url_query(&full), 80).to_string()
}

pub(crate) fn target_for(raw: &str) -> Option<DeepLinkTarget> {
    let rest = strip_scheme(raw.trim())?;

    // The graft, not a rewrite of the original. The untrusted half can only ever
    // land in the path and query of an origin we chose.
    let mut target = Url::parse(&format!("https://{HOST_DESKTOP}/{rest}")).ok()?;

    // The graft is not the guarantee. A remainder carrying its own authority
    // would reparse to a different host, and that is the one thing dropped here.
    if target.scheme() != "https" || target.host_str() != Some(HOST_DESKTOP) {
        crate::vprintln!("[deep_link] refused: {} does not stay on our origin", raw);
        return None;
    }

    match route_of(&target) {
        Route::Refused => {
            crate::vprintln!("[deep_link] refused {}: not a content route", target.path());
            return None;
        }
        // Already one slash and nothing was matched, leaving nothing to settle.
        // The home page is the one route with no segment of its own.
        Route::Home => {}
        Route::Content(path) => target.set_path(&path),
    }

    Some(DeepLinkTarget(target))
}

/// TIDAL's own content routes, first path segment only, from its router table.
///
/// `view` and `upload` are deliberately absent, both handing a whole query
/// string to code no one here has read. They belong in this list once someone has.
const CONTENT_SEGMENTS: &[&str] = &[
    "album",
    "android-upload",
    "artist",
    "browse",
    "credits",
    "edit-profile",
    "feed",
    "folder",
    "home",
    "import-playlist",
    "index.html",
    "mix",
    "my-collection",
    "not-found",
    "pick-artists",
    "playlist",
    "saved-uploads",
    "search",
    "settings",
    "spotlight",
    "track",
    "transfer-music",
    "uploads",
    "user",
    "video",
    "your-tracks",
    "your-uploads",
];

/// What the first path segment answers. Three answers and not two. The bare
/// scheme and a doubled slash both leave no first segment to read, and reading
/// both as the home page is what handed the allowlist a way round itself.
enum Route {
    /// The bare scheme, which opens what an ordinary launch opens.
    Home,
    /// A listed content route, carrying the path to deliver for it.
    Content(String),
    Refused,
}

/// Decoded and case-folded before matching, because TIDAL's router does both;
/// every one of its routes compiles case-insensitively and it percent-decodes
/// each segment first. `/LOGIN` and `/%6cogin` reach the same page there, and
/// comparing raw text here would let either spelling past.
///
/// The matched spelling comes back out, because it is the one that travels.
/// Checking one spelling and delivering another is the whole defect this answers
/// twice over: it let a checked `album` arrive as the host of `//album/1`, and
/// it pointed a launch at `/Album/1`, which TIDAL's own host answers with a 403.
///
/// Only that first segment is settled. Nothing past it was checked; nothing
/// past it is touched. Ids and share codes are case-sensitive, and rewriting an
/// escape there would change what it means.
fn route_of(target: &Url) -> Route {
    let path = target.path();
    // The one path carrying no segment to check.
    if path == "/" {
        return Route::Home;
    }
    let Some(after_root) = path.strip_prefix('/') else {
        return Route::Refused;
    };
    // An empty first segment is a second slash at the front, whatever spelling
    // put it there: `///`, a backslash a special scheme reads as a separator, or
    // a `..` that walked onto one.
    let Some(first) = after_root.split('/').next().filter(|s| !s.is_empty()) else {
        return Route::Refused;
    };
    let segment = percent_encoding::percent_decode_str(first)
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    if !CONTENT_SEGMENTS.contains(&segment.as_str()) {
        return Route::Refused;
    }
    Route::Content(format!("/{segment}{}", &after_root[first.len()..]))
}

/// The URL an OS launch put on our command line. Windows and Linux both deliver
/// one this way; macOS never does, sending an Apple Event to the running app
/// instead. Switches and the program name cannot match the scheme, needing no
/// separate skipping.
///
/// Takes `OsString` because `env::args()` panics on an argument that is not
/// valid Unicode, and one arriving beside our link is not an error to report.
/// Not being the URL, it is skipped like any other foreign argument.
pub(crate) fn url_from_args<I: IntoIterator<Item = OsString>>(args: I) -> Option<String> {
    args.into_iter()
        .filter_map(|arg| arg.into_string().ok())
        .find(|arg| strip_scheme(arg.trim()).is_some())
}

/// Looser than `strip_scheme` deliberately. `tidal:album/1` carries no authority
/// and the shell routes it here all the same, where a test keyed on `://` would
/// leave it ungated.
///
/// Compiled under `test` too: the rule is platform-neutral, its refusal is not.
#[cfg(any(target_os = "windows", test))]
fn names_scheme(arg: &str) -> bool {
    let Some((head, rest)) = arg.trim().split_at_checked(SCHEME.len()) else {
        return false;
    };
    head.eq_ignore_ascii_case(SCHEME) && rest.starts_with(':')
}

/// The shell substitutes exactly one value for `%1`; a launch made for a link
/// carries exactly one token. Anything past that came out of a quote inside the
/// value, the split `open_command` describes.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq)]
pub(crate) enum Launch {
    /// Nothing names the scheme: an ordinary start, whatever switches it carries.
    NoLink,
    /// Behind the `--` the handler registers, or without it.
    LinkAlone,
    /// A link, and tokens that were never ours to receive.
    LinkAndMore,
}

/// Split from the launch so the rule is exercised without a process to start.
///
/// Keyed on the link rather than on the `--type=` switch that tells a browser
/// process from a child: that flag answers a question the same broken quote can
/// answer too, and a routing decision is no place to hang a refusal.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn classify_launch(args: &[String]) -> Launch {
    let carried = match args.split_first() {
        Some((separator, rest)) if separator.as_str() == "--" => rest,
        _ => args,
    };
    if !carried.iter().any(|arg| names_scheme(arg)) {
        return Launch::NoLink;
    }
    if carried.len() == 1 {
        Launch::LinkAlone
    } else {
        Launch::LinkAndMore
    }
}

/// A validated target waiting for a window to show it. Three consumers drain it
/// through `take_pending`, whichever runs first winning and the others finding
/// it empty: the startup navigation, the browser's own registration, and the
/// task posted for a link that arrives while the app already runs.
static PENDING: Mutex<Option<DeepLinkTarget>> = Mutex::new(None);

/// Hand an OS-delivered URL to the running window, or hold it for the window
/// that is still being built. Refused URLs are dropped here, once; no caller
/// has to know the rule.
///
/// The window comes forward either way. Whether a link is acted on is decided
/// here, and the sender that handed it over cannot know that answer, so it does
/// not try. A refused link still answers the click with a window instead of
/// nothing at all.
pub(crate) fn deliver(raw: &str) {
    if let Some(target) = target_for(raw) {
        crate::vprintln!("[deep_link] accepted {}", for_log(&target));
        // Last one wins, as a browser does with two clicked links. Saying so
        // beats dropping the first without a word.
        if let Some(replaced) = PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(target)
        {
            crate::vprintln!(
                "[deep_link] {} superseded before it was shown",
                for_log(&replaced)
            );
        }
    }

    // Posted whatever `target_for` answered, and that is load-bearing everywhere:
    // a link that lands carries no focus signal beside it on Windows or Linux, and
    // macOS is handed nothing but this task. Gating this on an accepted link would
    // leave a refused click answering with nothing at all.
    //
    // Correctness of the link itself does not rest on the readiness flag;
    // `on_after_created` drains the slot when the browser appears, and a link
    // that slips through before then is applied rather than stranded.
    if crate::app_state::context_ready() {
        let mut task = NavigateTask::new(0);
        post_task(ThreadId::UI, Some(&mut task));
    }
}

/// Called by the startup navigation to open on the link instead of the home page.
pub(crate) fn take_pending() -> Option<DeepLinkTarget> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Called when the browser is registered, which is the fact a navigation waits
/// for. The readiness flag names an earlier one, the UI thread, letting a link
/// reach `deliver` after the startup navigation has drained the slot and before
/// there is anything to navigate; without this drain it would sit there unread.
pub(crate) fn apply_pending() {
    let Some(target) = take_pending() else {
        return;
    };
    navigate(&target);
    crate::ui::app_window::AppWindow::raise_current();
}

/// Routes inside the running page rather than reloading it. A document load
/// would rebuild the app around the link and throw away what it was showing.
///
/// The fallback is in the script, not around it. `__TL_NAVIGATE__` comes from
/// our own injected bundle, so a page without it is one that never received it,
/// and there `location.assign` does what a document load did before. The
/// failure lands on the old behaviour instead of on nothing.
fn navigate(target: &DeepLinkTarget) {
    let path = target.route();
    // The path is attacker-supplied past its first segment, and `eval_js` takes
    // injection-safety as its callers' contract.
    let script = format!(
        "(window.__TL_NAVIGATE__ ?? (p => location.assign(p)))({});",
        crate::app_state::js_string_literal(path)
    );
    if !crate::app_state::eval_js(&script) {
        crate::vprintln!("[deep_link] no frame to route {path} into");
    }
}

wrap_task! {
    struct NavigateTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            // Before anything else, and whether or not there is a link to show.
            // A click that was refused must still bring the app forward.
            crate::ui::app_window::AppWindow::raise_current();
            // Left in place when the browser is not up yet, since `apply_pending`
            // drains the same slot once it is.
            if crate::app_state::with_state(|s| s.browser.is_some()) != Some(true) {
                return;
            }
            let Some(target) = take_pending() else {
                return;
            };
            navigate(&target);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/deep_link.rs"]
mod tests;
