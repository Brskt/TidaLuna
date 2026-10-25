//! Plugin transpiling in a short-lived child of this executable.
//!
//! oxc recurses on the syntax tree with no depth guard: 3 KB of nested brackets overflow a 2 MiB
//! stack, and the overflow aborts the process past any `catch_unwind`, at every launch once enabled.
//! In a child the abort is an exit status, and the plugin is refused like any source that will not compile.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

/// The whole command line of a helper launch after the program name. Matched exactly: a token
/// smuggled into a longer command line, a deep link's, must not turn a launch into a helper.
const MARKER: &str = "--tidalunar-transpile-helper";

/// Under the updater's 30 s wait for this executable to exit, which an orphaned helper would hold.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Room for a transpiled bundle; the source is capped at 16 MiB before it is ever stored.
const OUTPUT_CAP: u64 = 64 << 20;

/// The helper transpiles on this stack. It only raises the depth that aborts; the child is what
/// makes an abort harmless.
const STACK: usize = 64 << 20;

/// What the error text keeps of the transpiler's message on its way to the log and the card.
const ERROR_TEXT_CAP: usize = 200;

/// The answer's first byte: JS follows, or the transpiler's refusal.
const ANSWER_JS: u8 = 0;
const ANSWER_REFUSED: u8 = 1;

/// The helper's exit code when it could not read its request or write its answer: its own
/// failure, not the source's.
const EXIT_BROKEN: i32 = 2;
/// The helper's exit code when its own clock ran out on the source.
const EXIT_TIMEOUT: i32 = 3;

/// Why an isolated transpile produced nothing.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The plugin's source was refused, crashed the transpiler, or outran the timeout or the cap.
    Source(String),
    /// The helper could not run or answered nonsense, which says nothing about the plugin.
    Helper(String),
}

impl Failure {
    /// Either clock running out: the parent's wait or the helper's own.
    fn timed_out() -> Failure {
        Failure::Source(format!(
            "the transpile took longer than {} s",
            TIMEOUT.as_secs()
        ))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Source(why) | Failure::Helper(why) => f.write_str(why),
        }
    }
}

/// True for a launch whose only argument is the helper marker.
pub(crate) fn is_helper_launch(mut args: impl Iterator<Item = OsString>) -> bool {
    let _program = args.next();
    args.next().is_some_and(|arg| arg == MARKER) && args.next().is_none()
}

/// The helper's whole life: one request on stdin, one answer on stdout. It touches no log, no
/// database and no app state, since the source it reads is a plugin's.
pub(crate) fn run() -> ! {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        // Without it, Windows Error Reporting holds a crashed helper for seconds before it exits.
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
    std::thread::spawn(|| {
        std::thread::sleep(TIMEOUT);
        std::process::exit(EXIT_TIMEOUT);
    });

    let mut request = Vec::new();
    if std::io::stdin().read_to_end(&mut request).is_err() {
        std::process::exit(EXIT_BROKEN);
    }
    let Some((filename, source)) = decode_request(&request) else {
        std::process::exit(EXIT_BROKEN);
    };
    let (filename, source) = (filename.to_string(), source.to_string());
    let transpiled = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            super::transpile::transpile_ts(&source, &filename).map_err(|e| e.to_string())
        });
    let answer = match transpiled.map(|thread| thread.join()) {
        Ok(Ok(Ok(js))) => encode_answer(ANSWER_JS, &js),
        Ok(Ok(Err(refused))) => encode_answer(ANSWER_REFUSED, &refused),
        Ok(Err(_panic)) => encode_answer(ANSWER_REFUSED, "the transpiler panicked on this source"),
        Err(_) => std::process::exit(EXIT_BROKEN),
    };

    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(&answer)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        std::process::exit(EXIT_BROKEN);
    }
    std::process::exit(0)
}

/// Transpile `source` in a helper process. One helper at a time: `plugin.enable` is reachable
/// from page code, and every call would otherwise be a process.
pub(crate) fn transpile(source: &str, filename: &str) -> Result<String, Failure> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    let exe = std::env::current_exe()
        .map_err(|e| Failure::Helper(format!("cannot name this executable: {e}")))?;
    let mut command = Command::new(exe);
    command
        .arg(MARKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = Reaped(
        command
            .spawn()
            .map_err(|e| Failure::Helper(format!("cannot start the transpile helper: {e}")))?,
    );
    let (Some(mut stdin), Some(stdout)) = (child.0.stdin.take(), child.0.stdout.take()) else {
        return Err(Failure::Helper(
            "the transpile helper has no pipes".to_string(),
        ));
    };

    let header = request_header(filename);
    let answer = std::thread::scope(|scope| {
        // On its own thread: a helper wedged before reading would block this write past the timeout.
        scope.spawn(move || {
            // A helper that died early closed the pipe; its exit status says why, not this write.
            let _ = stdin
                .write_all(&header)
                .and_then(|()| stdin.write_all(source.as_bytes()));
        });
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            let mut answer = Vec::new();
            let read = stdout.take(OUTPUT_CAP + 1).read_to_end(&mut answer);
            let _ = tx.send(read.map(|_| answer));
        });
        let answer = rx.recv_timeout(TIMEOUT);
        if answer.is_err() {
            // Closes both pipes, which is what lets the two threads above end.
            let _ = child.0.kill();
        }
        answer
    });

    let answer = match answer {
        Err(_) => return Err(Failure::timed_out()),
        Ok(Err(e)) => {
            return Err(Failure::Helper(format!(
                "cannot read the transpile helper: {e}"
            )));
        }
        Ok(Ok(answer)) => answer,
    };
    if answer.len() as u64 > OUTPUT_CAP {
        return Err(Failure::Source(format!(
            "the transpiled code exceeds {} MiB",
            OUTPUT_CAP >> 20
        )));
    }
    let status = child
        .0
        .wait()
        .map_err(|e| Failure::Helper(format!("cannot reap the transpile helper: {e}")))?;
    decode_answer(status.success(), status.code(), &answer)
}

/// Kills and reaps the helper on every way out, a panic included: a `Child` dropped alive runs on.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The request is the filename's length (u32 little endian), the filename, then the source to EOF.
fn request_header(filename: &str) -> Vec<u8> {
    let mut header = (filename.len() as u32).to_le_bytes().to_vec();
    header.extend_from_slice(filename.as_bytes());
    header
}

fn decode_request(request: &[u8]) -> Option<(&str, &str)> {
    let (len, rest) = request.split_first_chunk::<4>()?;
    let (filename, source) = rest.split_at_checked(u32::from_le_bytes(*len) as usize)?;
    Some((
        std::str::from_utf8(filename).ok()?,
        std::str::from_utf8(source).ok()?,
    ))
}

fn encode_answer(kind: u8, body: &str) -> Vec<u8> {
    let mut answer = Vec::with_capacity(1 + body.len());
    answer.push(kind);
    answer.extend_from_slice(body.as_bytes());
    answer
}

/// Sorts the helper's exit and answer. A crash or the helper's own timeout is the source's doing;
/// a helper that could not do its job, or answered nothing readable, is not.
fn decode_answer(exited_ok: bool, code: Option<i32>, answer: &[u8]) -> Result<String, Failure> {
    if !exited_ok {
        return Err(match code {
            Some(EXIT_BROKEN) => {
                Failure::Helper("the transpile helper could not do its job".to_string())
            }
            Some(EXIT_TIMEOUT) => Failure::timed_out(),
            _ => Failure::Source("the transpiler crashed on this source".to_string()),
        });
    }
    match answer.split_first() {
        Some((&ANSWER_JS, js)) => String::from_utf8(js.to_vec())
            .map_err(|_| Failure::Helper("the transpile helper answered no text".to_string())),
        Some((&ANSWER_REFUSED, why)) => Err(Failure::Source(
            crate::util::truncate_str(&String::from_utf8_lossy(why), ERROR_TEXT_CAP).to_string(),
        )),
        _ => Err(Failure::Helper(
            "the transpile helper gave no answer".to_string(),
        )),
    }
}
