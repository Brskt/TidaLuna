//! Cross-platform render-crash recovery dialog.
//!
//! Shown when the main page's render process dies. A separate CEF window (its own
//! browser, isolated from the dead renderer) built on the shared `super::dialog`
//! helper. Buttons navigate to crash:// URLs, mapped to a `CrashAction`.

use std::sync::atomic::AtomicBool;

use cef::*;
use tokio::sync::oneshot;

use crate::ui::dialog::{escape_html, show_dialog};

const CRASH_RELOAD: &str = "crash://reload";
const CRASH_OPEN: &str = "crash://open";
const CRASH_QUIT: &str = "crash://quit";
const DIALOG_W: i32 = 520;
const DIALOG_H: i32 = 300;

/// Set while a crash dialog is on screen: a fresh crash (e.g. the reloaded page
/// dies again) must not stack a second dialog. Reset once the dialog resolves.
pub(crate) static CRASH_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashAction {
    Reload,
    OpenFolder,
    Quit,
}

/// Show the crash dialog and return the user's choice via a oneshot channel.
/// Posts to the CEF UI thread internally; the receiver must be awaited off the
/// UI thread.
pub(crate) fn show_crash_dialog(
    reason: &str,
    error_code: i32,
    log_path: Option<&std::path::Path>,
) -> oneshot::Receiver<CrashAction> {
    let html = build_html(reason, error_code, log_path);
    show_dialog(html, (DIALOG_W, DIALOG_H), parse_crash, CrashAction::Reload)
}

/// Closing without a button is a reload (`show_dialog`'s on_close).
fn parse_crash(url: &str) -> Option<CrashAction> {
    if url.starts_with(CRASH_RELOAD) {
        Some(CrashAction::Reload)
    } else if url.starts_with(CRASH_OPEN) {
        Some(CrashAction::OpenFolder)
    } else if url.starts_with(CRASH_QUIT) {
        Some(CrashAction::Quit)
    } else {
        None
    }
}

fn build_html(reason: &str, error_code: i32, log_path: Option<&std::path::Path>) -> String {
    let log_line = match log_path {
        Some(p) => format!(
            r#"<div class="log">Crash log saved to: {}</div>"#,
            escape_html(&p.display().to_string())
        ),
        None => String::new(),
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
.dialog {{ max-width:460px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:14px; color:#eb1e32; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.log {{ font-size:11px; color:#777; word-break:break-all; margin-top:6px; }}
.actions {{ display:flex; gap:8px; justify-content:flex-end; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px;
}}
.quit {{ background:#333; }}
.quit:hover {{ background:#444; }}
.open {{ background:#333; }}
.open:hover {{ background:#444; }}
.reload {{ background:#eb1e32; }}
.reload:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<div class="dialog">
    <h2>TidaLunar page crashed</h2>
    <div class="info">
        The page process {reason} (error code {code}).
        {log_line}
    </div>
    <div class="actions">
        <button class="quit" onclick="location.href='{quit}'">Quit</button>
        <button class="open" onclick="location.href='{open}'">Open crash folder</button>
        <button class="reload" onclick="location.href='{reload}'">Reload</button>
    </div>
</div>
</body>
</html>"#,
        reason = escape_html(reason),
        code = error_code,
        log_line = log_line,
        quit = CRASH_QUIT,
        open = CRASH_OPEN,
        reload = CRASH_RELOAD,
    )
}

// --- Task to reload the main browser on the UI thread ---

wrap_task! {
    pub(crate) struct ReloadMainTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            if let Some(browser) = crate::app_state::with_state(|s| s.browser.clone()).flatten() {
                browser.reload();
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/crash_dialog.rs"]
mod tests;
