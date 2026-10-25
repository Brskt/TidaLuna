//! Tests for `src/ui/destination_dialog.rs`, attached by `#[path]`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use cef::FileDialogMode;
use tokio::sync::oneshot;

use super::{Dialogs, Request, Verdict, choose_destination, classify};
use crate::ui::file_dialog::NotRecorded;

/// Dialogs that answer from a script.
struct Scripted {
    answers: Mutex<VecDeque<Vec<String>>>,
    replies: Mutex<VecDeque<i32>>,
}

impl Scripted {
    fn new(answers: &[&[&str]], replies: &[i32]) -> Scripted {
        Scripted {
            answers: Mutex::new(
                answers
                    .iter()
                    .map(|paths| paths.iter().map(|p| p.to_string()).collect())
                    .collect(),
            ),
            replies: Mutex::new(replies.iter().copied().collect()),
        }
    }
}

impl Dialogs for Scripted {
    fn file(
        &self,
        _mode: FileDialogMode,
        _title: Option<String>,
        _default_path: Option<String>,
        _filters: Vec<String>,
    ) -> oneshot::Receiver<Vec<String>> {
        let (tx, rx) = oneshot::channel();
        if let Some(answer) = self.answers.lock().unwrap().pop_front() {
            let _ = tx.send(answer);
        }
        rx
    }

    fn message(
        &self,
        _title: &str,
        _message: &str,
        _detail: &str,
        _buttons: &[String],
        _default_id: i32,
        _cancel_id: i32,
    ) -> oneshot::Receiver<i32> {
        let (tx, rx) = oneshot::channel();
        if let Some(reply) = self.replies.lock().unwrap().pop_front() {
            let _ = tx.send(reply);
        }
        rx
    }
}

const FORBIDDEN: &str = "/install/x.dll";
const ALSO_FORBIDDEN: &str = "/install/y.dll";
const ALLOWED: &str = "/home/me/x.json";

fn judge(answer: Vec<String>) -> Verdict {
    classify(answer, |path| {
        if path.starts_with("/install") {
            Err(NotRecorded::Protected)
        } else {
            Ok(())
        }
    })
}

fn save() -> Request {
    Request {
        mode: FileDialogMode::SAVE,
        title: None,
        default_path: Some("x.json".to_string()),
        filters: Vec::new(),
    }
}

const CHOOSE_ANOTHER: i32 = 0;
const CANCEL: i32 = 1;

/// A blob has no later gate: a path the vetting cannot resolve is refused like a protected one,
/// and a refused path anywhere in the answer refuses it.
#[test]
fn classify_refuses_the_first_path_it_cannot_admit() {
    let answer = vec![
        ALLOWED.to_string(),
        FORBIDDEN.to_string(),
        ALSO_FORBIDDEN.to_string(),
    ];
    assert_eq!(
        judge(answer),
        Verdict::Refused(PathBuf::from(FORBIDDEN), NotRecorded::Protected)
    );
    assert_eq!(
        classify(vec!["../x".to_string()], |_| Err(NotRecorded::Unresolvable)),
        Verdict::Refused(PathBuf::from("../x"), NotRecorded::Unresolvable)
    );
}

/// Whatever the user does after a refusal, the refused path never reaches the plugin.
#[tokio::test]
async fn a_refused_path_never_reaches_the_plugin() {
    let dialogs = Scripted::new(&[&[FORBIDDEN], &[ALLOWED]], &[CHOOSE_ANOTHER]);
    assert_eq!(choose_destination(&dialogs, save(), judge).await, [ALLOWED]);

    let dialogs = Scripted::new(&[&[FORBIDDEN]], &[CANCEL]);
    assert!(choose_destination(&dialogs, save(), judge).await.is_empty());
}
