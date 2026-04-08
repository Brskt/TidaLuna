//! Bun child process manager for native plugin modules.
//!
//! Plugins can include `.native.ts` files — CJS bundles targeting Node.js.
//! In upstream TidaLuna (Electron), these run in Node.js main process.
//! Here we spawn a Bun child process and communicate via JSON lines on stdin/stdout.
//!
//! Lifecycle:
//!   1. First `__Luna.registerNative` IPC → lazy-spawn Bun (via OnceLock in state.rs)
//!   2. Send `register` command with module code → Bun evals, returns export names
//!   3. Plugin calls `__LunaNative.{name}` IPC → `call` command to Bun → result back
//!   4. App exit → tokio runtime dropped → stdin/stdout tasks cancelled →
//!      pipes closed → native-host.cjs readline emits 'close' → process.exit(0)

pub(crate) mod trust;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>>>;

pub struct NativeRuntime {
    stdin_tx: mpsc::UnboundedSender<String>,
    pending: PendingMap,
    next_id: AtomicU64,
    _child: Child,
}

impl NativeRuntime {
    /// The native-host sandbox script, embedded at compile time.
    /// Never written to disk — sent to Bun via stdin at spawn.
    const HOST_SCRIPT: &str = include_str!("../../frontend/scripts/native-host.cjs");

    /// Bootstrap: Function() wrapper provides a proper CJS module scope
    /// (require/module/exports) that direct execution wouldn't have.
    const BOOTSTRAP: &str = r#"let b=[];const r=require("readline").createInterface({input:process.stdin});globalThis.__rl=r;r.on("line",function h(l){if(l==="__END__"){r.removeListener("line",h);new Function("require","module","exports","__dirname","__filename",b.join("\n"))(require,{exports:{}},{},"/","[stdin]")}else b.push(l)})"#;

    pub fn spawn(rt: &tokio::runtime::Handle) -> anyhow::Result<Self> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        // Find bundled Bun binary — no PATH fallback (fail closed)
        let bun_path = find_binary(&exe_dir, if cfg!(windows) { "bun.exe" } else { "bun" })
            .ok_or_else(|| {
                anyhow::anyhow!("Bun binary not found. Native plugin modules require Bun in bin/.")
            })?;

        crate::vprintln!("[BUN]    Spawning: {}", bun_path.display());

        let mut cmd = Command::new(&bun_path);
        cmd.arg("-e")
            .arg(Self::BOOTSTRAP)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Hygiene: strip inherited env, only pass safe vars (matches upstream TidaLuna).
        // This is NOT a security boundary — absolute paths and OS APIs remain accessible.
        cmd.env_clear();
        for key in [
            // Upstream TidaLuna whitelist (temp/home/path)
            "TMPDIR",
            "TMP",
            "TEMP",
            "HOME",
            "USERPROFILE",
            "PATH",
            "APPDATA",
            // Windows system vars (required for DLL loading, crypto, etc.)
            "SystemRoot",
            "WINDIR",
            "SYSTEMDRIVE",
            "COMSPEC",
            "PATHEXT",
            "PROGRAMDATA",
        ] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        cmd.current_dir(std::env::temp_dir());

        // Hide the console window on Windows
        #[cfg(target_os = "windows")]
        {
            #[allow(unused_imports)]
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn Bun: {e}"))?;

        let stdout = child.stdout.take().expect("stdout captured");
        let stderr = child.stderr.take().expect("stderr captured");
        let stdin = child.stdin.take().expect("stdin captured");

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();

        // Task: send embedded sandbox script, then relay IPC commands to stdin
        rt.spawn(async move {
            let mut writer = BufWriter::new(stdin);

            // Send the embedded native-host.cjs sandbox script
            if writer
                .write_all(Self::HOST_SCRIPT.as_bytes())
                .await
                .is_err()
                || writer.write_all(b"\n__END__\n").await.is_err()
                || writer.flush().await.is_err()
            {
                return;
            }

            while let Some(line) = stdin_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        // Task: read responses from stdout
        let pending_clone = pending.clone();
        rt.spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => {
                        crate::vprintln!(
                            "[BUN]    Invalid JSON from stdout: {}",
                            &line[..line.len().min(200)]
                        );
                        continue;
                    }
                };

                let id = parsed
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }

                let result = if let Some(err) = parsed.get("error") {
                    Err(err.as_str().unwrap_or("unknown error").to_string())
                } else {
                    Ok(parsed)
                };

                let tx = pending_clone
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&id));
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }

            // EOF — Bun process exited. Drain all pending with errors.
            if let Ok(mut map) = pending_clone.lock() {
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err("Bun process exited".to_string()));
                }
            }
        });

        // Task: forward stderr to vprintln
        rt.spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                crate::vprintln!("[BUN]    {}", line);
            }
        });

        Ok(Self {
            stdin_tx,
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        })
    }

    /// Send a command to the Bun process and return a receiver for the response.
    ///
    /// This is synchronous — safe to call under a lock. The caller `.await`s the
    /// receiver outside the lock to get the result.
    ///
    /// The command JSON must NOT contain an `"id"` field — one is assigned
    /// automatically and injected into the command before sending.
    pub fn send_command(
        &self,
        mut cmd: serde_json::Value,
    ) -> Result<oneshot::Receiver<Result<serde_json::Value, String>>, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        cmd["id"] = serde_json::Value::String(id.clone());

        let (tx, rx) = oneshot::channel();
        match self.pending.lock() {
            Ok(mut map) => {
                map.insert(id.clone(), tx);
            }
            Err(e) => return Err(format!("pending lock poisoned: {e}")),
        }

        let line = serde_json::to_string(&cmd).unwrap_or_default();
        if self.stdin_tx.send(line).is_err() {
            match self.pending.lock() {
                Ok(mut map) => {
                    map.remove(&id);
                }
                Err(e) => {
                    crate::vprintln!("[NATIVE] pending lock poisoned during cleanup: {e}");
                }
            }
            return Err("Bun stdin channel closed".to_string());
        }

        Ok(rx)
    }
}

/// Find a binary next to the executable, or fall back to bare name (resolved via PATH by OS).
fn find_binary(exe_dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    // bin/ subdirectory (new layout)
    let bin = exe_dir.join("bin").join(name);
    if bin.exists() {
        return Some(bin);
    }
    // Flat layout (backward compat)
    let local = exe_dir.join(name);
    if local.exists() {
        return Some(local);
    }
    // No PATH fallback — fail closed for security
    None
}
