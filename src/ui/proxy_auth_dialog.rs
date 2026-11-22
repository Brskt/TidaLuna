//! Sign-in to an authenticating proxy, in a window of our own. Chromium's prompt is switched off
//! (`disable-chrome-login-prompt`): it also answered any server's 401, which a script in the page
//! can provoke with a host of its choosing.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use cef::{AuthCallback, CefString, ImplAuthCallback};
use tokio::sync::oneshot;

use crate::ui::dialog::{FIT_TO_CONTENT, escape_html, show_dialog};

const SCHEME: &str = "authcred";
const SUBMIT_URL: &str = "authcred://submit";
const CANCEL_URL: &str = "authcred://cancel";
const DIALOG_W: i32 = 460;
const DIALOG_H: i32 = 420;

/// What an authentication challenge may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthPolicy {
    Refuse,
    AskUser,
}

impl AuthPolicy {
    /// A server never gets a prompt. A proxy does only for a tunnelled request: on `http://`, a
    /// GET-mode proxy forwards an origin's 407 under its own host, and could forward the answer back.
    pub(crate) fn for_challenge(is_proxy: bool, origin_url: &str) -> Self {
        let tunnelled = url::Url::parse(origin_url)
            .is_ok_and(|parsed| matches!(parsed.scheme(), "https" | "wss"));
        if is_proxy && tunnelled {
            Self::AskUser
        } else {
            Self::Refuse
        }
    }
}

/// The proxy's sign-in. No `Debug`: a `{:?}` would print the password.
#[derive(Clone)]
pub(crate) struct ProxyCredentials {
    username: String,
    password: String,
}

#[derive(Clone)]
pub(crate) enum ProxyAuthReply {
    Credentials(ProxyCredentials),
    Cancelled,
}

/// Every `authcred://` navigation answers and none reaches the network. A submit missing its
/// username, or giving a field twice, cancels: it never becomes empty credentials.
fn parse_reply(url: &str) -> Option<ProxyAuthReply> {
    let parsed = url::Url::parse(url).ok().filter(|u| u.scheme() == SCHEME)?;
    if parsed.host_str() != Some("submit") {
        return Some(ProxyAuthReply::Cancelled);
    }
    Some(credentials_from(&parsed).map_or(ProxyAuthReply::Cancelled, ProxyAuthReply::Credentials))
}

fn credentials_from(parsed: &url::Url) -> Option<ProxyCredentials> {
    let (mut username, mut password) = (None, None);
    for (key, value) in parsed.query_pairs() {
        let slot = match key.as_ref() {
            "u" => &mut username,
            "p" => &mut password,
            _ => return None,
        };
        if slot.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    Some(ProxyCredentials {
        username: username.filter(|u: &String| !u.is_empty())?,
        password: password?,
    })
}

/// A challenge as CEF raised it, copied out of the handler's borrowed strings.
pub(crate) struct Challenge {
    pub(crate) is_proxy: bool,
    pub(crate) origin_url: String,
    pub(crate) host: String,
    pub(crate) port: i32,
    pub(crate) realm: String,
    pub(crate) scheme: String,
}

/// Realm and scheme only tell challenges apart: the window shows neither, the server wrote both.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ChallengeKey {
    host: String,
    port: i32,
    realm: String,
    scheme: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Joined {
    /// The first waiter: it opens the window.
    First,
    /// A window for this key is already up; its answer reaches this waiter too.
    Waiting,
}

/// Requests waiting on one sign-in. Join and settle share the lock: no waiter is left behind.
pub(crate) struct Pending<W> {
    waiting: Mutex<HashMap<ChallengeKey, Vec<W>>>,
}

impl<W> Pending<W> {
    pub(crate) fn new() -> Self {
        Self {
            waiting: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn join(&self, key: ChallengeKey, waiter: W) -> Joined {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let queue = waiting.entry(key).or_default();
        queue.push(waiter);
        if queue.len() == 1 {
            Joined::First
        } else {
            Joined::Waiting
        }
    }

    /// Taken before the answer goes out: a rejected password makes Chromium ask again, and that
    /// request must open a new window, not join the one closing.
    pub(crate) fn settle(&self, key: &ChallengeKey) -> Vec<W> {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.remove(key).unwrap_or_default()
    }
}

static PENDING: LazyLock<Pending<AuthCallback>> = LazyLock::new(Pending::new);

/// Answers a challenge CEF raised on its IO thread without blocking it: a refusal cancels now, a
/// proxy sign-in keeps `callback` and answers once the window does.
pub(crate) fn answer_challenge(challenge: Challenge, callback: AuthCallback) {
    if AuthPolicy::for_challenge(challenge.is_proxy, &challenge.origin_url) == AuthPolicy::Refuse {
        crate::vprintln!(
            "[AUTH]   Refused a sign-in prompt (proxy={}) for {}",
            challenge.is_proxy,
            crate::util::refused_host(&challenge.origin_url)
        );
        callback.cancel();
        return;
    }
    let key = ChallengeKey {
        host: challenge.host,
        port: challenge.port,
        realm: challenge.realm,
        scheme: challenge.scheme,
    };
    if PENDING.join(key.clone(), callback) == Joined::Waiting {
        return;
    }
    let rx = show_proxy_auth_dialog(&key.host, key.port);
    crate::state::rt_handle().spawn(async move {
        let reply = rx.await.unwrap_or(ProxyAuthReply::Cancelled);
        for waiter in PENDING.settle(&key) {
            match &reply {
                ProxyAuthReply::Credentials(c) => waiter.cont(
                    Some(&CefString::from(c.username.as_str())),
                    Some(&CefString::from(c.password.as_str())),
                ),
                ProxyAuthReply::Cancelled => waiter.cancel(),
            }
        }
    });
}

fn show_proxy_auth_dialog(host: &str, port: i32) -> oneshot::Receiver<ProxyAuthReply> {
    show_dialog(
        build_html(host, port),
        (DIALOG_W, DIALOG_H),
        parse_reply,
        ProxyAuthReply::Cancelled,
    )
}

fn build_html(host: &str, port: i32) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
* {{ margin:0; padding:0; box-sizing:border-box; }}
body {{
    background:#1a1a1a; color:#fff; font-family:system-ui,sans-serif;
    display:flex; justify-content:center;
    padding:32px 0; -webkit-app-region:drag;
}}
.dialog {{ max-width:400px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:16px; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.info .source {{ color:#fff; word-break:break-all; }}
label {{ display:block; font-size:12px; color:#999; margin-bottom:4px; }}
input {{
    width:100%; padding:8px 10px; margin-bottom:12px; border:1px solid #333;
    border-radius:4px; background:#111; color:#fff; font-size:13px;
}}
.actions {{ display:flex; gap:8px; justify-content:flex-end; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px;
}}
.cancel {{ background:#333; }}
.cancel:hover {{ background:#444; }}
.submit {{ background:#eb1e32; }}
.submit:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<form class="dialog" onsubmit="event.preventDefault(); location.href='{submit_url}?u='+encodeURIComponent(this.u.value)+'&p='+encodeURIComponent(this.p.value);">
    <h2>Proxy Sign-in</h2>
    <div class="info">The proxy <span class="source">{host}:{port}</span> asks for a username and password.</div>
    <label for="u">Username</label>
    <input id="u" name="u" type="text" autocomplete="off" autofocus>
    <label for="p">Password</label>
    <input id="p" name="p" type="password" autocomplete="off">
    <div class="actions">
        <button type="button" class="cancel" onclick="location.href='{cancel_url}'">Cancel</button>
        <button type="submit" class="submit">Sign in</button>
    </div>
</form>
{fit}
</body>
</html>"#,
        host = escape_html(host),
        port = port,
        submit_url = SUBMIT_URL,
        cancel_url = CANCEL_URL,
        fit = FIT_TO_CONTENT,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/proxy_auth_dialog.rs"]
mod tests;
