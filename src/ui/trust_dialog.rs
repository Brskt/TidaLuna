//! Native trust dialog — separate CEF window, isolated from the main renderer.
//!
//! JS in the main TIDAL page cannot interact with this window:
//! different browser, different Client, no shared IPC channel.
//! Communication uses URL navigation (trust://allow, trust://deny)
//! intercepted by the dialog's own RequestHandler.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const TRUST_ALLOW: &str = "trust://allow";
const TRUST_DENY: &str = "trust://deny";

/// Show a trust dialog and return the user's decision via a oneshot channel.
/// Can be called from any thread — internally posts to the CEF UI thread.
pub(crate) fn show_trust_dialog(
    plugin_name: &str,
    module_name: &str,
    manifest_json: &str,
) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    let html = build_html(plugin_name, module_name, manifest_json);
    let sender = Arc::new(Mutex::new(Some(tx)));
    let mut task = ShowDialogTask::new(html, sender);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn build_html(plugin_name: &str, module: &str, manifest_json: &str) -> String {
    // "DiscordRPC/discord.native.ts" → plugin="DiscordRPC", file="discord.native.ts"
    // "@scope/pkg/foo.native.ts" → plugin="@scope/pkg", file="foo.native.ts"
    let (display_plugin, display_file) = match plugin_name.rsplit_once('/') {
        Some((p, f)) => (p, Some(f)),
        None => (plugin_name, None),
    };

    let (module_label, module_desc) = match module {
        "fs" | "fs/promises" => (
            "Filesystem",
            "Read, write, and delete files on your computer.",
        ),
        "child_process" => (
            "Process Spawning",
            "Run programs and shell commands on your computer.",
        ),
        "worker_threads" => (
            "Worker Threads",
            "Run JavaScript code in parallel background threads.",
        ),
        "cluster" => ("Cluster", "Spawn multiple copies of this process."),
        "os" => (
            "System Info",
            "Read system details: hostname, memory, CPU, user info.",
        ),
        "vm" => (
            "Code Execution",
            "Evaluate arbitrary JavaScript code in a new context.",
        ),
        "v8" => ("V8 Engine", "Access low-level JavaScript engine internals."),
        "inspector" => (
            "Debugger",
            "Attach a debugger to inspect and control this process.",
        ),
        other => (other, ""),
    };

    let manifest: serde_json::Value =
        serde_json::from_str(manifest_json).unwrap_or(serde_json::Value::Null);
    let author = manifest.get("author");
    let author_name = author
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let author_url = author
        .and_then(|a| a.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let avatar_url = author
        .and_then(|a| a.get("avatarUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let plugin_desc = manifest
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let author_block = if !author_name.is_empty() {
        let avatar_html = if !avatar_url.is_empty() {
            format!(
                r#"<img src="{src}" style="width:32px;height:32px;border-radius:50%;flex-shrink:0" onerror="this.style.display='none'">"#,
                src = escape_html(avatar_url)
            )
        } else if author_url.contains("github.com/") {
            // Derive GitHub avatar from author URL
            let gh_user = author_url
                .split("github.com/")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .unwrap_or("");
            if !gh_user.is_empty() {
                format!(
                    r#"<img src="https://github.com/{user}.png?size=64" style="width:32px;height:32px;border-radius:50%;flex-shrink:0" onerror="this.style.display='none'">"#,
                    user = escape_html(gh_user)
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let desc_html = if !plugin_desc.is_empty() {
            format!(
                r#"<div style="font-size:12px;color:#999;margin-top:2px">{}</div>"#,
                escape_html(plugin_desc)
            )
        } else {
            String::new()
        };

        format!(
            r#"<div style="display:flex;align-items:center;gap:10px;margin:0 0 14px;padding:10px 14px;background:#222;border-radius:4px">
        {avatar}
        <div>
            <div style="font-size:13px;color:#fff;font-weight:600">{name}</div>
            {desc}
        </div>
    </div>"#,
            avatar = avatar_html,
            name = escape_html(author_name),
            desc = desc_html
        )
    } else {
        String::new()
    };

    let file_html = display_file
        .map(|f| {
            format!(
                r#"<div><span class="label">File: </span><span>{file}</span></div>"#,
                file = escape_html(f)
            )
        })
        .unwrap_or_default();

    let desc_html = if !module_desc.is_empty() {
        format!(
            r#"<div style="margin-top:6px;padding-top:6px;border-top:1px solid #333;color:#aaa;font-size:12px">{desc}</div>"#,
            desc = escape_html(module_desc)
        )
    } else {
        String::new()
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
* {{ margin:0; padding:0; box-sizing:border-box; }}
body {{
    background:#1a1a1a; color:#fff; font-family:system-ui,sans-serif;
    display:flex; align-items:center; justify-content:center;
    height:100vh; -webkit-app-region:drag;
}}
.dialog {{ max-width:560px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:16px; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.info .label {{ color:#999; }}
.info .value {{ color:#fff; font-weight:600; }}
.info .access {{ color:#eb1e32; font-weight:600; }}
.warn {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:12px; color:#999; line-height:1.5; margin-bottom:16px;
}}
.warn ul {{ margin:4px 0 0 16px; padding:0; list-style:disc; }}
.actions {{ display:flex; gap:8px; justify-content:flex-end; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px;
}}
.deny {{ background:#333; }}
.deny:hover {{ background:#444; }}
.allow {{ background:#eb1e32; }}
.allow:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<div class="dialog">
    <h2>Plugin Permission Request</h2>
    {author_block}
    <div class="info">
        <div><span class="label">Plugin: </span><span class="value">{plugin}</span></div>
        {file_html}
        <div><span class="label">Requested access: </span><span class="access">{module_label}</span></div>
        {desc_html}
    </div>
    <div class="warn">
        Only allow if you trust this plugin.
        <div style="margin-top:6px">This decision will be remembered unless the plugin is:</div>
        <ul>
            <li>Reinstalled</li>
            <li>Updated</li>
        </ul>
    </div>
    <div class="actions">
        <button class="deny" onclick="location.href='{deny_url}'">Deny</button>
        <button class="allow" onclick="location.href='{allow_url}'">Allow</button>
    </div>
</div>
</body>
</html>"#,
        author_block = author_block,
        plugin = escape_html(display_plugin),
        file_html = file_html,
        module_label = escape_html(module_label),
        desc_html = desc_html,
        allow_url = TRUST_ALLOW,
        deny_url = TRUST_DENY,
    )
}

// --- Request handler: intercepts trust:// navigations ---

wrap_request_handler! {
    struct DialogRequestHandler {
        sender: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    crate::ui::token_filter::userfree_to_string(&u)
                })
                .unwrap_or_default();

            let granted = if url.starts_with(TRUST_ALLOW) {
                Some(true)
            } else if url.starts_with(TRUST_DENY) {
                Some(false)
            } else {
                None
            };

            if let Some(granted) = granted {
                if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(granted);
                }
                if let Some(b) = browser
                    && let Some(host) = b.host()
                {
                    host.try_close_browser();
                }
                return 1;
            }
            0
        }
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            None
        }
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
        }
    }
}

// --- Minimal life span handler ---

wrap_life_span_handler! {
    struct DialogLifeSpanHandler {
        _p: u8,
    }
    impl LifeSpanHandler {}
}

// --- Client ---

wrap_client! {
    struct DialogClient {
        life_span: LifeSpanHandler,
        request: RequestHandler,
    }
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
    }
}

// --- Window delegate ---

wrap_window_delegate! {
    struct TrustDialogWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        sender: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size {
                width: 620,
                height: 460,
            }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_resize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn can_maximize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn can_minimize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn on_window_created(&self, window: Option<&mut Window>) {
            let bv = self.browser_view.borrow();
            let (Some(window), Some(bv)) = (window, bv.as_ref()) else {
                return;
            };
            let mut view = View::from(bv);
            window.add_child_view(Some(&mut view));
            window.center_window(Some(&Size {
                width: 620,
                height: 460,
            }));

            if let Some(mut icon) = image_create() {
                let png_data = include_bytes!("../../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
            }

            window.show();
            window.activate();
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
            // Closing via X without clicking a button → deny
            if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send(false);
            }
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }
    }
}

wrap_browser_view_delegate! {
    struct DialogBrowserViewDelegate {
        _p: u8,
    }
    impl ViewDelegate {}
    impl BrowserViewDelegate {}
}

// --- Task to create the dialog on the CEF UI thread ---

wrap_task! {
    struct ShowDialogTask {
        html: String,
        sender: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    }
    impl Task {
        fn execute(&self) {
            let life_span = DialogLifeSpanHandler::new(0);
            let request = DialogRequestHandler::new(self.sender.clone());
            let mut client = DialogClient::new(life_span, request);

            let settings = BrowserSettings {
                background_color: 0xFF1A1A1A,
                ..Default::default()
            };

            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(self.html.as_bytes());
            let data_url = format!("data:text/html;base64,{b64}");
            let url = CefString::from(data_url.as_str());

            let mut bv_delegate = DialogBrowserViewDelegate::new(0);
            let browser_view = browser_view_create(
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut bv_delegate),
            );

            let mut window_delegate = TrustDialogWindowDelegate::new(
                RefCell::new(browser_view),
                self.sender.clone(),
            );
            window_create_top_level(Some(&mut window_delegate));
        }
    }
}
