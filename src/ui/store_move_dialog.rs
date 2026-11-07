//! Native confirmation of a store move, in a window of its own like the install dialog: the move
//! changes where the listed plugins update from, and any script in the page can ask for one.

use tokio::sync::oneshot;

use crate::plugins::store::StoreMove;
use crate::ui::dialog::{FIT_TO_CONTENT, OnBlur, escape_html, show_dialog};

const MOVE_ALLOW: &str = "store-move://allow";
const MOVE_DENY: &str = "store-move://deny";
const DIALOG_W: i32 = 620;
const DIALOG_H: i32 = 520;

/// `moves` are the rows Rust read, never what the caller said. Stays open when it loses focus: it
/// comes up at launch, while the main window takes the focus. Only a button answers: a close, by
/// the user, another process or the app quitting, decides nothing. Callable from any thread.
pub(crate) fn show_store_move_dialog(
    from_base: &str,
    to_base: &str,
    moves: &[StoreMove],
) -> oneshot::Receiver<Option<bool>> {
    let html = build_html(from_base, to_base, moves);
    show_dialog(html, (DIALOG_W, DIALOG_H), parse_move, None, OnBlur::Stay)
}

fn parse_move(url: &str) -> Option<Option<bool>> {
    if url.starts_with(MOVE_ALLOW) {
        Some(Some(true))
    } else if url.starts_with(MOVE_DENY) {
        Some(Some(false))
    } else {
        None
    }
}

fn build_html(from_base: &str, to_base: &str, moves: &[StoreMove]) -> String {
    let plugins: String = moves
        .iter()
        .map(|m| format!("<li>{}</li>", escape_html(&m.name)))
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
    padding:32px 0; -webkit-app-region:drag;
}}
.dialog {{ max-width:560px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:16px; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.info .label {{ color:#999; }}
.info .source {{ color:#fff; word-break:break-all; }}
.info ul {{ margin:6px 0 0 18px; color:#fff; font-weight:600; }}
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
    <h2>Plugin Store Moved</h2>
    <div class="info">
        <div><span class="label">From: </span><span class="source">{from}</span></div>
        <div><span class="label">To: </span><span class="source">{to}</span></div>
        <div><span class="label">Plugins that will update from the new address:</span></div>
        <ul>{plugins}</ul>
    </div>
    <div class="warn">
        These plugins will take their updates from the new address. Only accept if you trust it.
        Cancel turns them off. Closing this window decides nothing: they run from the old address
        and you are asked again at the next launch.
    </div>
    <div class="actions">
        <button class="deny" onclick="location.href='{deny_url}'">Cancel</button>
        <button class="allow" onclick="location.href='{allow_url}'">Move</button>
    </div>
</div>
{fit}
</body>
</html>"#,
        from = escape_html(from_base),
        to = escape_html(to_base),
        allow_url = MOVE_ALLOW,
        deny_url = MOVE_DENY,
        fit = FIT_TO_CONTENT,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/store_move_dialog.rs"]
mod tests;
