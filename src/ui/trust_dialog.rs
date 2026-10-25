//! Native trust dialog - a separate CEF window isolated from the main renderer.
//!
//! JS in the main TIDAL page cannot interact with this window: different
//! browser, different Client, no shared IPC channel. One row per access the
//! module asks for, each answered on its own; Validate navigates to
//! trust://submit/<one 0 or 1 per row>, intercepted by the shared dialog request
//! handler (see `super::dialog`).

use tokio::sync::oneshot;

use crate::ui::dialog::{FIT_TO_CONTENT, OnBlur, author_block, escape_html, show_dialog};

const TRUST_SUBMIT: &str = "trust://submit/";
const DIALOG_W: i32 = 620;
/// A ceiling: the window shrinks to the page, which is held to the window and lets its row list
/// scroll rather than push Validate out of it.
const DIALOG_H: i32 = 640;

/// How a trust dialog ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrustReply {
    /// One answer per row, in the order shown: true allows.
    Answered(Vec<bool>),
    /// Closed without Validate: nothing was decided, and the module will be asked again.
    Closed,
}

/// Show a trust dialog for `rows` and return the user's answers via a oneshot channel.
/// Can be called from any thread - internally posts to the CEF UI thread.
pub(crate) fn show_trust_dialog(
    plugin_name: &str,
    rows: &[&str],
    manifest_json: &str,
) -> oneshot::Receiver<TrustReply> {
    let html = build_html(plugin_name, rows, manifest_json);
    show_dialog(
        html,
        (DIALOG_W, DIALOG_H),
        parse_trust,
        TrustReply::Closed,
        OnBlur::Stay,
    )
}

/// The answers carried by a Validate navigation. The page is ours, but the count is not checked
/// here: the caller knows how many rows it showed. Anything else under the scheme ends the dialog
/// as closed rather than leaving a dead button.
fn parse_trust(url: &str) -> Option<TrustReply> {
    let bits = url.strip_prefix(TRUST_SUBMIT)?.trim_end_matches('/');
    let answers: Option<Vec<bool>> = bits
        .chars()
        .map(|c| match c {
            '1' => Some(true),
            '0' => Some(false),
            _ => None,
        })
        .collect();
    Some(match answers {
        Some(answers) if !answers.is_empty() => TrustReply::Answered(answers),
        _ => TrustReply::Closed,
    })
}

/// The label and description of a row, by the row names `native.rs` asks with.
fn row_text(row: &str) -> (&str, &'static str) {
    match row {
        "net" => (
            "Network Access",
            "Open connections and make requests to any server (TCP, HTTP, TLS, DNS).",
        ),
        "fs" => (
            "Filesystem",
            "Read, write, and delete files on your computer.",
        ),
        "os" => (
            "System Info",
            "Read system details: hostname, memory, CPU, user info.",
        ),
        "dgram" => ("UDP Sockets", "Send and receive UDP network packets."),
        "diagnostics_channel" => (
            "Diagnostics",
            "Observe internal events such as HTTP requests and DNS queries.",
        ),
        "worker_threads" => (
            "Worker Threads",
            "Use inter-thread communication APIs (messaging, ports).",
        ),
        other => (other, ""),
    }
}

fn build_html(plugin_name: &str, rows: &[&str], manifest_json: &str) -> String {
    // "DiscordRPC/discord.native.ts" -> plugin="DiscordRPC", file="discord.native.ts"
    // "@scope/pkg/foo.native.ts" -> plugin="@scope/pkg", file="foo.native.ts"
    let (display_plugin, display_file) = match plugin_name.rsplit_once('/') {
        Some((p, f)) => (p, Some(f)),
        None => (plugin_name, None),
    };

    let author_block = author_block(manifest_json);

    let file_html = display_file
        .map(|f| {
            format!(
                r#"<div><span class="label">File: </span><span>{file}</span></div>"#,
                file = escape_html(f)
            )
        })
        .unwrap_or_default();

    let rows_html: String = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let (label, desc) = row_text(row);
            format!(
                r#"<div class="row">
            <div class="what"><div class="name">{label}</div><div class="desc">{desc}</div></div>
            <label><input type="radio" name="r{i}" value="0"> Deny</label>
            <label><input type="radio" name="r{i}" value="1"> Allow</label>
        </div>"#,
                label = escape_html(label),
                desc = escape_html(desc),
            )
        })
        .collect();

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
    padding:24px 0; -webkit-app-region:drag;
}}
.dialog {{
    max-width:560px; width:90%; -webkit-app-region:no-drag;
    max-height:calc(100vh - 48px); display:flex; flex-direction:column;
}}
h2 {{ font-size:16px; margin-bottom:12px; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:12px;
}}
.info .label {{ color:#999; }}
.info .value {{ color:#fff; font-weight:600; }}
.rows {{ flex:1 1 auto; min-height:0; overflow-y:auto; margin-bottom:12px; }}
.row {{
    display:flex; align-items:center; gap:12px;
    background:#222; padding:8px 14px; border-radius:4px; margin-bottom:6px;
    font-size:13px; color:#ccc;
}}
.row .what {{ flex:1; }}
.row .name {{ color:#eb1e32; font-weight:600; }}
.row .desc {{ color:#999; font-size:12px; }}
.row label {{ white-space:nowrap; cursor:pointer; }}
.warn {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:12px; color:#999; line-height:1.5; margin-bottom:12px;
}}
.warn ul {{ margin:4px 0 0 16px; padding:0; list-style:disc; }}
.actions {{ display:flex; justify-content:flex-end; }}
button {{
    padding:8px 16px; border:none; border-radius:4px; background:#eb1e32;
    color:#fff; cursor:pointer; font-size:13px;
}}
button:hover {{ background:#d11a2d; }}
button:disabled {{ background:#333; color:#777; cursor:default; }}
</style>
</head>
<body>
<div class="dialog">
    <h2>Plugin Permission Request</h2>
    {author_block}
    <div class="info">
        <div><span class="label">Plugin: </span><span class="value">{plugin}</span></div>
        {file_html}
    </div>
    <div class="rows">
        {rows_html}
    </div>
    <div class="warn">
        Only allow what you trust this plugin with.
        <div style="margin-top:6px">Your answers are remembered unless the plugin is:</div>
        <ul>
            <li>Reinstalled</li>
            <li>Updated</li>
        </ul>
        <div style="margin-top:6px">Closing this window answers nothing: you will be asked again.</div>
    </div>
    <div class="actions">
        <button id="validate" disabled>Validate</button>
    </div>
</div>
<script>
const count = {count};
const validate = document.getElementById("validate");
function answers() {{
    let bits = "";
    for (let i = 0; i < count; i++) {{
        const chosen = document.querySelector('input[name="r' + i + '"]:checked');
        if (!chosen) return null;
        bits += chosen.value;
    }}
    return bits;
}}
document.addEventListener("change", () => {{ validate.disabled = answers() === null; }});
validate.addEventListener("click", () => {{
    const bits = answers();
    if (bits !== null) location.href = "{submit}" + bits;
}});
</script>
{fit}
</body>
</html>"#,
        author_block = author_block,
        plugin = escape_html(display_plugin),
        file_html = file_html,
        rows_html = rows_html,
        count = rows.len(),
        submit = TRUST_SUBMIT,
        fit = FIT_TO_CONTENT,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/trust_dialog.rs"]
mod tests;
