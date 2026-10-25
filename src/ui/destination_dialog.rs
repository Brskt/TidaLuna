//! A save or folder dialog whose answer the install guard may refuse. The user is told and picks
//! again or gives up, as Chromium does for a folder its File System Access API blocks: CEF has no
//! hook to refuse inside the native dialog, which has closed by the time the answer is judged.

use std::path::{Path, PathBuf};

use cef::FileDialogMode;
use tokio::sync::oneshot;

use super::file_dialog::NotRecorded;

/// The two dialogs a turn opens, behind a seam that lets tests run the loop without CEF.
pub(crate) trait Dialogs: Sync {
    fn file(
        &self,
        mode: FileDialogMode,
        title: Option<String>,
        default_path: Option<String>,
        filters: Vec<String>,
    ) -> oneshot::Receiver<Vec<String>>;

    fn message(
        &self,
        title: &str,
        message: &str,
        detail: &str,
        buttons: &[String],
        default_id: i32,
        cancel_id: i32,
    ) -> oneshot::Receiver<i32>;
}

pub(crate) struct Native;

impl Dialogs for Native {
    fn file(
        &self,
        mode: FileDialogMode,
        title: Option<String>,
        default_path: Option<String>,
        filters: Vec<String>,
    ) -> oneshot::Receiver<Vec<String>> {
        super::file_dialog::show_file_dialog(mode, title, default_path, filters)
    }

    fn message(
        &self,
        title: &str,
        message: &str,
        detail: &str,
        buttons: &[String],
        default_id: i32,
        cancel_id: i32,
    ) -> oneshot::Receiver<i32> {
        super::message_dialog::show_message_dialog(
            title, message, detail, buttons, default_id, cancel_id,
        )
    }
}

/// What one dialog answer amounts to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Cancelled,
    /// The first path turned down, and why.
    Refused(PathBuf, NotRecorded),
    /// The answer as the dialog gave it: the caller hands it on unchanged.
    Chosen(Vec<String>),
}

/// `vet` is the site's own judgement of one path. Whatever it does not admit is refused, an
/// unresolvable path included: a blob download has no later gate that would refuse it.
pub(crate) fn classify(
    answer: Vec<String>,
    vet: impl Fn(&Path) -> Result<(), NotRecorded>,
) -> Verdict {
    if answer.is_empty() {
        return Verdict::Cancelled;
    }
    for raw in &answer {
        if let Err(why) = vet(Path::new(raw)) {
            return Verdict::Refused(PathBuf::from(raw), why);
        }
    }
    Verdict::Chosen(answer)
}

/// One dialog, opened the same way on every turn.
pub(crate) struct Request {
    pub mode: FileDialogMode,
    pub title: Option<String>,
    pub default_path: Option<String>,
    pub filters: Vec<String>,
}

const CHOOSE_ANOTHER: i32 = 0;
const CANCEL: i32 = 1;

/// Open the dialog, judge the answer, and on a refusal say why and reopen until the user picks an
/// admitted location or cancels (empty result). Turns are uncapped: a count would turn repicking
/// the install folder into a silent cancel.
pub(crate) async fn choose_destination<D, J>(dialogs: &D, request: Request, judge: J) -> Vec<String>
where
    D: Dialogs,
    J: Fn(Vec<String>) -> Verdict + Clone + Send + 'static,
{
    loop {
        let answer = dialogs
            .file(
                request.mode,
                request.title.clone(),
                request.default_path.clone(),
                request.filters.clone(),
            )
            .await
            .unwrap_or_default();
        // Off the async worker, since judging resolves the path on disk.
        let judge = judge.clone();
        let Ok(verdict) = tokio::task::spawn_blocking(move || judge(answer)).await else {
            return Vec::new();
        };
        let (path, why) = match verdict {
            Verdict::Cancelled => return Vec::new(),
            Verdict::Chosen(paths) => return paths,
            Verdict::Refused(path, why) => (path, why),
        };
        crate::vprintln!(
            "[DIALOG] Refused {} ({why:?}), asking again",
            path.display()
        );
        let message = match why {
            NotRecorded::Protected => {
                "This folder holds TidaLunar's own files. Choose a location outside it."
            }
            NotRecorded::Unresolvable => "TidaLunar cannot use this location. Choose another one.",
        };
        let buttons = ["Choose another location".to_string(), "Cancel".to_string()];
        let reply = dialogs
            .message(
                "",
                message,
                &path.display().to_string(),
                &buttons,
                CHOOSE_ANOTHER,
                CANCEL,
            )
            .await;
        // A message that never answers stops too; reopening on no reply would loop unseen.
        if reply != Ok(CHOOSE_ANOTHER) {
            return Vec::new();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/destination_dialog.rs"]
mod tests;
