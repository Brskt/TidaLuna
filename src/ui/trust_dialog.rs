//! Native trust dialog - a separate CEF window isolated from the main renderer.
//!
//! JS in the main TIDAL page cannot interact with this window: different
//! browser, different Client, no shared IPC channel. Buttons navigate to
//! trust://allow / trust://deny, intercepted by the shared dialog request
//! handler (see `super::dialog`).

use tokio::sync::oneshot;

use crate::ui::dialog::{escape_html, show_dialog};

const TRUST_ALLOW: &str = "trust://allow";
const TRUST_DENY: &str = "trust://deny";
const DIALOG_W: i32 = 620;
const DIALOG_H: i32 = 460;

/// Show a trust dialog and return the user's decision via a oneshot channel.
/// Can be called from any thread - internally posts to the CEF UI thread.
pub(crate) fn show_trust_dialog(
    plugin_name: &str,
    module_name: &str,
    manifest_json: &str,
) -> oneshot::Receiver<bool> {
    let html = build_html(plugin_name, module_name, manifest_json);
    show_dialog(html, (DIALOG_W, DIALOG_H), parse_trust, false)
}

/// Closing without a button is denial (`show_dialog`'s on_close = false).
fn parse_trust(url: &str) -> Option<bool> {
    if url.starts_with(TRUST_ALLOW) {
        Some(true)
    } else if url.starts_with(TRUST_DENY) {
        Some(false)
    } else {
        None
    }
}

fn build_html(plugin_name: &str, module: &str, manifest_json: &str) -> String {
    // "DiscordRPC/discord.native.ts" -> plugin="DiscordRPC", file="discord.native.ts"
    // "@scope/pkg/foo.native.ts" -> plugin="@scope/pkg", file="foo.native.ts"
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
            "Use inter-thread communication APIs (messaging, ports).",
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
        "diagnostics_channel" => (
            "Diagnostics",
            "Observe internal events such as HTTP requests and DNS queries.",
        ),
        "dgram" => ("UDP Sockets", "Send and receive UDP network packets."),
        "net" | "http" | "https" | "http2" | "tls" | "dns" | "dns/promises" => (
            "Network Access",
            "Open connections and make requests to any server (TCP, HTTP, TLS, DNS).",
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

#[cfg(test)]
#[path = "../../tests/unit/ui/trust_dialog.rs"]
mod tests;
