//! Native message/error dialog - a separate CEF window isolated from the main
//! renderer, built on the shared `super::dialog` helper. Buttons navigate to
//! `msgbox://<index>`, mapped to the clicked index. Backs `showMessageBox` and
//! `showErrorBox` from `@luna/lib.native`.

use tokio::sync::oneshot;

use crate::ui::dialog::{FIT_TO_CONTENT, escape_html, show_dialog};

const MSGBOX_SCHEME: &str = "msgbox://";
const DIALOG_W: i32 = 580;
/// A ceiling: the window shrinks to its content.
const DIALOG_H: i32 = 400;

/// Show a message dialog and return the clicked button index via a oneshot.
/// `cancel_id` is returned if the window is closed without a button click.
/// Can be called from any thread - internally posts to the CEF UI thread.
pub(crate) fn show_message_dialog(
    title: &str,
    message: &str,
    detail: &str,
    buttons: &[String],
    default_id: i32,
    cancel_id: i32,
) -> oneshot::Receiver<i32> {
    let html = build_html(title, message, detail, buttons, default_id);
    show_dialog(html, (DIALOG_W, DIALOG_H), parse_msgbox, cancel_id)
}

fn parse_msgbox(url: &str) -> Option<i32> {
    url.strip_prefix(MSGBOX_SCHEME)
        .and_then(|s| s.parse::<i32>().ok())
}

/// The message's line breaks, kept as Electron keeps them: a blank line starts a paragraph, a
/// single break stays a break.
fn paragraphs(message: &str) -> String {
    message
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(|p| p.trim_matches('\n'))
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", escape_html(p).replace('\n', "<br>")))
        .collect()
}

fn build_html(
    title: &str,
    message: &str,
    detail: &str,
    buttons: &[String],
    default_id: i32,
) -> String {
    let title_html = if title.is_empty() {
        String::new()
    } else {
        format!(r#"<h2>{}</h2>"#, escape_html(title))
    };

    let message_html = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="message">{}</div>"#, paragraphs(message))
    };

    let detail_html = if detail.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="detail">{}</div>"#, escape_html(detail))
    };

    let buttons_html = buttons
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let cls = if i as i32 == default_id {
                "btn primary"
            } else {
                "btn"
            };
            format!(
                r#"<button class="{cls}" onclick="location.href='{scheme}{i}'">{label}</button>"#,
                cls = cls,
                scheme = MSGBOX_SCHEME,
                i = i,
                label = escape_html(label)
            )
        })
        .collect::<Vec<_>>()
        .join("");

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
.dialog {{ max-width:540px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:12px; }}
.message {{ font-size:14px; color:#eee; line-height:1.5; margin-bottom:12px; }}
.message p + p {{ margin-top:10px; }}
.detail {{
    background:#222; padding:8px 12px; border-radius:4px;
    font-size:12px; color:#ccc; line-height:1.5; word-break:break-all;
}}
.actions {{ display:flex; gap:8px; justify-content:flex-end; margin-top:16px; flex-wrap:wrap; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px; background:#333;
}}
button:hover {{ background:#444; }}
button.primary {{ background:#eb1e32; }}
button.primary:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<div class="dialog">
    {title_html}
    {message_html}
    {detail_html}
    <div class="actions">{buttons_html}</div>
</div>
{fit}
</body>
</html>"#,
        title_html = title_html,
        message_html = message_html,
        detail_html = detail_html,
        buttons_html = buttons_html,
        fit = FIT_TO_CONTENT,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/ui/message_dialog.rs"]
mod tests;
