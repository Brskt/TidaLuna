//! Native install confirmation, in a window of its own like the trust dialog: the plugin store and
//! every installed plugin run in the same V8 context, and a click in the page cannot tell the user
//! from a plugin.

use tokio::sync::oneshot;

use crate::ui::dialog::{FIT_TO_CONTENT, OnBlur, author_block, escape_html, show_dialog};

const INSTALL_ALLOW: &str = "install://allow";
const INSTALL_DENY: &str = "install://deny";
const DIALOG_W: i32 = 620;
const DIALOG_H: i32 = 460;

/// `name` and `manifest_json` are what Rust fetched from `url`, never what the caller said.
/// Callable from any thread: it posts to the CEF UI thread.
pub(crate) fn show_install_dialog(
    name: &str,
    url: &str,
    manifest_json: &str,
) -> oneshot::Receiver<bool> {
    let html = build_html(name, url, manifest_json);
    show_dialog(
        html,
        (DIALOG_W, DIALOG_H),
        parse_install,
        false,
        OnBlur::Close,
    )
}

/// Closing without a button, or clicking outside the window, declines (`on_close` = false).
fn parse_install(url: &str) -> Option<bool> {
    if url.starts_with(INSTALL_ALLOW) {
        Some(true)
    } else if url.starts_with(INSTALL_DENY) {
        Some(false)
    } else {
        None
    }
}

fn build_html(name: &str, url: &str, manifest_json: &str) -> String {
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
.dialog {{ max-width:560px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:16px; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.info .label {{ color:#999; }}
.info .value {{ color:#fff; font-weight:600; }}
.info .source {{ color:#fff; word-break:break-all; }}
.warn {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:12px; color:#999; line-height:1.5; margin-bottom:16px;
}}
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
    <h2>Install Plugin</h2>
    {author_block}
    <div class="info">
        <div><span class="label">Plugin: </span><span class="value">{name}</span></div>
        <div><span class="label">Source: </span><span class="source">{url}</span></div>
    </div>
    <div class="warn">
        A plugin runs inside TidaLunar and can use your TIDAL account. Only install it if you
        trust where it comes from. If you did not ask to install it, cancel.
    </div>
    <div class="actions">
        <button class="deny" onclick="location.href='{deny_url}'">Cancel</button>
        <button class="allow" onclick="location.href='{allow_url}'">Install</button>
    </div>
</div>
{fit}
</body>
</html>"#,
        author_block = author_block(manifest_json),
        name = escape_html(name),
        url = escape_html(url),
        allow_url = INSTALL_ALLOW,
        deny_url = INSTALL_DENY,
        fit = FIT_TO_CONTENT,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/install_dialog.rs"]
mod tests;
