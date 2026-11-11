//! Linux: the XDG FileChooser portal, on a thread of its own since its D-Bus exchange blocks for as
//! long as the user browses. Without a usable portal the CEF dialog takes over, slow walk included.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::message::MatchRule;

use super::Answer;
use crate::util::one_at_a_time::OneAtATime;

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";
const CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const REQUEST: &str = "org.freedesktop.portal.Request";
/// `directory` arrived in version 3 of the interface.
const MIN_VERSION: u32 = 3;
static TOKENS: AtomicU32 = AtomicU32::new(0);

enum Outcome {
    Chosen(String),
    Cancelled,
    /// No portal, one too old, or no FileChooser backend: CEF's dialog answers instead.
    Unavailable(String),
    /// The dialog was shown and the exchange then broke.
    Failed(String),
}

pub(super) fn open(
    guard: OneAtATime,
    owner: usize,
    title: Option<String>,
    default_path: Option<String>,
    slot: Answer,
) {
    let in_thread = slot.clone();
    let fallback_title = title.clone();
    let fallback_path = default_path.clone();
    let spawned = std::thread::Builder::new()
        .name("folder-portal".into())
        .spawn(move || {
            let outcome = pick(
                owner,
                title.as_deref().unwrap_or(""),
                default_path.as_deref(),
            );
            drop(guard);
            match outcome {
                Outcome::Chosen(path) => in_thread.send(vec![path]),
                Outcome::Cancelled => in_thread.send(Vec::new()),
                Outcome::Failed(why) => {
                    crate::verr!("[DIALOG] Folder portal failed: {why}");
                    in_thread.send(Vec::new());
                }
                Outcome::Unavailable(why) => {
                    crate::vprintln!("[DIALOG] No folder portal ({why}), using CEF's dialog");
                    crate::ui::file_dialog::show_cef_folder_dialog(title, default_path, in_thread);
                }
            }
        });
    if let Err(e) = spawned {
        crate::verr!("[DIALOG] Could not start the folder portal thread: {e}");
        crate::ui::file_dialog::show_cef_folder_dialog(fallback_title, fallback_path, slot);
    }
}

/// `x11:<hex XID>`, or empty when there is no window to name, as the portal asks.
pub(super) fn parent_window(xid: usize) -> String {
    if xid == 0 {
        String::new()
    } else {
        format!("x11:{xid:x}")
    }
}

/// The object path the portal answers on, known before the call so the reply cannot be missed.
pub(super) fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{OBJECT}/request/{sender}/{token}")
}

/// The one `file://` URI a folder choice returns, as a path. `None` for anything else, a name that
/// is not UTF-8 included: a lossy conversion would name another folder.
pub(super) fn uri_to_path(uris: &[&str]) -> Option<String> {
    let [uri] = uris else {
        return None;
    };
    let encoded = uri.strip_prefix("file://")?;
    let bytes: Vec<u8> = percent_encoding::percent_decode_str(encoded).collect();
    String::from_utf8(bytes)
        .ok()
        .filter(|path| path.starts_with('/'))
}

/// Whether a `NameOwnerChanged` says the portal's own connection closed, as a unique name does when
/// its process exits. Its well-known name may change hands while that process lives on.
pub(super) fn portal_left(name: &str, old_owner: &str, new_owner: &str, portal: &str) -> bool {
    name == portal && old_owner == portal && new_owner.is_empty()
}

/// What the portal's `Response` comes to: 1 is the user cancelling, 2 a backend that failed.
fn response(code: u32, results: &PropMap) -> Outcome {
    match code {
        0 => {}
        1 => return Outcome::Cancelled,
        other => return Outcome::Failed(format!("the dialog ended with response {other}")),
    }
    let uris: Vec<&str> = results
        .get("uris")
        .and_then(|uris| uris.0.as_iter())
        .map(|items| items.filter_map(|item| item.as_str()).collect())
        .unwrap_or_default();
    if uris.is_empty() {
        return Outcome::Cancelled;
    }
    match uri_to_path(&uris) {
        Some(path) => Outcome::Chosen(path),
        None => Outcome::Failed(format!("unusable answer {uris:?}")),
    }
}

fn pick(owner: usize, title: &str, default_path: Option<&str>) -> Outcome {
    // Without a session bus libdbus would try to autolaunch one.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return Outcome::Unavailable("no session bus".into());
    }
    let unavailable = |e: dbus::Error| Outcome::Unavailable(e.to_string());
    let conn = match Connection::new_session() {
        Ok(conn) => conn,
        Err(e) => return unavailable(e),
    };

    // The portal is D-Bus activated: it may not be running yet.
    let bus = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_secs(5),
    );
    let started: Result<(u32,), _> = bus.method_call(
        "org.freedesktop.DBus",
        "StartServiceByName",
        (SERVICE, 0u32),
    );
    if let Err(e) = started {
        return unavailable(e);
    }
    // Only the portal's own name may answer, not another process of the session.
    let owner_name: Result<(String,), _> =
        bus.method_call("org.freedesktop.DBus", "GetNameOwner", (SERVICE,));
    let owner_name = match owner_name {
        Ok((name,)) => name,
        Err(e) => return unavailable(e),
    };

    // Addressed to that process, not to the name: a portal restarted since would answer from a
    // name the Response rule ignores, where this way the call fails and CEF's dialog takes over.
    let portal = conn.with_proxy(owner_name.as_str(), OBJECT, Duration::from_secs(25));
    let version: u32 = match portal.get(CHOOSER, "version") {
        Ok(version) => version,
        Err(e) => return unavailable(e),
    };
    if version < MIN_VERSION {
        return Outcome::Unavailable(format!("FileChooser v{version}"));
    }

    let token = format!("tidalunar_{}", TOKENS.fetch_add(1, Ordering::Relaxed));
    let handle = request_path(&conn.unique_name(), &token);
    let Ok(path) = dbus::Path::new(handle.clone()) else {
        return Outcome::Unavailable(format!("bad request path {handle}"));
    };
    let reply: Arc<Mutex<Option<(u32, PropMap)>>> = Arc::default();
    let sink = reply.clone();
    let rule = MatchRule::new_signal(REQUEST, "Response")
        .with_path(path)
        .with_sender(owner_name.clone())
        .static_clone();
    // Subscribed before the call: the portal may answer at once.
    if let Err(e) = conn.add_match(rule, move |response: (u32, PropMap), _, _| {
        *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(response);
        false
    }) {
        return unavailable(e);
    }
    // Also before the call: a portal exiting from here on is either refused the call or heard.
    let gone = Arc::new(AtomicBool::new(false));
    let left = gone.clone();
    let watched = owner_name.clone();
    let watch = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
        .with_sender("org.freedesktop.DBus");
    let watching = conn.add_match(
        watch,
        move |(name, old, new): (String, String, String), _, _| {
            if portal_left(&name, &old, &new, &watched) {
                left.store(true, Ordering::Release);
            }
            true
        },
    );
    if let Err(e) = watching {
        return unavailable(e);
    }

    let mut options: PropMap = HashMap::new();
    options.insert("handle_token".into(), Variant(Box::new(token)));
    options.insert("directory".into(), Variant(Box::new(true)));
    options.insert("modal".into(), Variant(Box::new(true)));
    if let Some(start) = default_path {
        let mut bytes = start.as_bytes().to_vec();
        bytes.push(0);
        options.insert("current_folder".into(), Variant(Box::new(bytes)));
    }
    let opened: Result<(dbus::Path<'static>,), _> =
        portal.method_call(CHOOSER, "OpenFile", (parent_window(owner), title, options));
    match opened {
        Ok((returned,)) if *returned == *handle => {}
        // Portals before 0.9 answer elsewhere; they predate FileChooser v3, refused above.
        Ok((returned,)) => {
            return Outcome::Unavailable(format!("answer at {returned}, not {handle}"));
        }
        Err(e) => return unavailable(e),
    }

    let take = || reply.lock().unwrap_or_else(|e| e.into_inner()).take();
    loop {
        if let Err(e) = conn.process(Duration::from_millis(500)) {
            return Outcome::Failed(e.to_string());
        }
        if let Some((code, results)) = take() {
            return response(code, &results);
        }
        if gone.load(Ordering::Acquire) {
            // Whatever it sent before exiting is read first: a Response there still wins.
            while conn.process(Duration::ZERO).unwrap_or(false) {}
            return match take() {
                Some((code, results)) => response(code, &results),
                None => Outcome::Failed("the portal exited while the dialog was open".into()),
            };
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/folder_picker/linux.rs"]
mod tests;
