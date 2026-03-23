//! Bun child process manager for native plugin modules.
//!
//! Plugins can include `.native.ts` files — CJS bundles targeting Node.js.
//! In upstream TidaLuna (Electron), these run in Node.js main process.
//! Here we spawn a Bun child process and communicate via JSON lines on stdin/stdout.
//!
//! Lifecycle:
//!   1. First `__Luna.registerNative` IPC → lazy-spawn Bun
//!   2. Send `register` command with module code → Bun evals, returns export names
//!   3. Plugin calls `__LunaNative.{name}` IPC → `call` command to Bun → result back
//!   4. App exit → drop NativeRuntime → kill Bun process

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
    /// Spawn a Bun child process running `native-host.cjs`.
    ///
    /// Looks for the Bun binary and host script next to the current executable,
    /// falling back to `bun` in PATH for development.
    pub fn spawn(rt: &tokio::runtime::Handle) -> anyhow::Result<Self> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        // Find Bun binary
        let bun_path = find_binary(&exe_dir, if cfg!(windows) { "bun.exe" } else { "bun" })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Bun binary not found. Native plugin modules require Bun in dist/ or PATH."
                )
            })?;

        // Find host script
        let host_script = find_file(&exe_dir, "native-host.cjs")
            .ok_or_else(|| anyhow::anyhow!("native-host.cjs not found next to executable."))?;

        crate::vprintln!(
            "[BUN]    Spawning: {} run {}",
            bun_path.display(),
            host_script.display()
        );

        let mut child = Command::new(&bun_path)
            .arg("run")
            .arg(&host_script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn Bun: {e}"))?;

        let stdout = child.stdout.take().expect("stdout captured");
        let stderr = child.stderr.take().expect("stderr captured");
        let stdin = child.stdin.take().expect("stdin captured");

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();

        // Task: write commands to stdin
        rt.spawn(async move {
            let mut writer = BufWriter::new(stdin);
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

                if let Ok(mut map) = pending_clone.lock()
                    && let Some(tx) = map.remove(&id)
                {
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

    /// Register a native module. Returns the list of exported names.
    #[allow(dead_code)]
    pub async fn register(&self, name: &str, code: &str) -> Result<Vec<String>, String> {
        let cmd = serde_json::json!({
            "id": self.next_id(),
            "type": "register",
            "name": name,
            "code": code,
        });

        let response = self.send_and_wait(cmd).await?;

        let exports = response
            .get("exports")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(exports)
    }

    /// Call a function on a registered native module. Returns the JSON result.
    #[allow(dead_code)]
    pub async fn call(
        &self,
        module_name: &str,
        fn_name: &str,
        args: &[serde_json::Value],
    ) -> Result<serde_json::Value, String> {
        let cmd = serde_json::json!({
            "id": self.next_id(),
            "type": "call",
            "name": module_name,
            "fn": fn_name,
            "args": args,
        });

        let response = self.send_and_wait(cmd).await?;

        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Clone the stdin sender (for use across await boundaries without holding state).
    pub fn stdin_tx_clone(&self) -> mpsc::UnboundedSender<String> {
        self.stdin_tx.clone()
    }

    /// Clone the pending map (for use across await boundaries).
    pub fn pending_clone(&self) -> PendingMap {
        self.pending.clone()
    }

    /// Get the next request ID as a string.
    pub fn next_id_str(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    #[allow(dead_code)]
    fn next_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    #[allow(dead_code)]
    async fn send_and_wait(&self, cmd: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = cmd
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string();
        let (tx, rx) = oneshot::channel();

        {
            let mut map = self.pending.lock().map_err(|e| format!("lock: {e}"))?;
            map.insert(id, tx);
        }

        let line = serde_json::to_string(&cmd).map_err(|e| format!("serialize: {e}"))?;
        self.stdin_tx
            .send(line)
            .map_err(|_| "Bun stdin channel closed".to_string())?;

        rx.await
            .map_err(|_| "Bun response channel dropped".to_string())?
    }
}

/// Find a binary next to the executable, or fall back to bare name (resolved via PATH by OS).
fn find_binary(exe_dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    let local = exe_dir.join(name);
    if local.exists() {
        return Some(local);
    }
    // Fallback: bare name — OS resolves via PATH (for dev environments with bun installed)
    Some(PathBuf::from(name))
}

/// Find a file next to the executable.
fn find_file(exe_dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    let path = exe_dir.join(name);
    if path.exists() { Some(path) } else { None }
}
