//! Process-wide clipboard writer.
//!
//! On X11 the clipboard contents are served by the owning process, so a
//! transient `Clipboard` dropped right after `set_text` loses the selection on
//! minimal window managers. A single long-lived owner thread keeps one
//! `arboard::Clipboard` alive for the process lifetime and serializes writes,
//! which also sidesteps `Clipboard` not being `Send`.

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

static WRITER: OnceLock<Sender<String>> = OnceLock::new();

fn writer() -> &'static Sender<String> {
    WRITER.get_or_init(|| {
        let (tx, rx) = channel::<String>();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                // Lazily create the clipboard so a transient init failure (no
                // display yet) doesn't permanently kill the channel - retry on
                // the next write until it succeeds, then keep it alive.
                let mut clipboard: Option<arboard::Clipboard> = None;
                while let Ok(text) = rx.recv() {
                    if clipboard.is_none() {
                        match arboard::Clipboard::new() {
                            Ok(c) => clipboard = Some(c),
                            Err(e) => {
                                crate::vprintln!("[CLIP]   init failed: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(clipboard) = clipboard.as_mut()
                        && let Err(e) = clipboard.set_text(text)
                    {
                        crate::vprintln!("[CLIP]   set_text failed: {e}");
                    }
                }
            })
            .expect("spawn clipboard thread");
        tx
    })
}

/// Queue a clipboard text write on the owner thread. Fire-and-forget.
pub(crate) fn write_text(text: String) {
    let _ = writer().send(text);
}
