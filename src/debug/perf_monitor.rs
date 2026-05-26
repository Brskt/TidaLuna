//! Opt-in live performance sampler feeding the in-app perf overlay.
//!
//! When `TIDALUNAR_PERF` is set, a background thread samples per-CEF-subprocess
//! CPU and memory via `sysinfo` and pushes a snapshot to the page over IPC,
//! where `frontend/src/debug/perf-overlay.ts` graphs it. Unset = no thread, no
//! cost. This exists to watch consumption live and confirm a fix lands, since
//! the DevTools monitor can't break CPU/RAM down per CEF process.

use std::thread;
use std::time::Duration;

use serde::Serialize;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::connect::ipc::post_emit_with_data;

/// 500 ms is below the eye's tolerance for a live graph and above sysinfo's
/// minimum interval for a meaningful per-process CPU delta.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const PERF_CHANNEL: &str = "perf.sample";

#[derive(Serialize)]
struct ProcInfo {
    /// CEF process role from its `--type=` switch ("renderer", "gpu-process",
    /// "utility", …); the main browser process has no such switch.
    kind: String,
    pid: u32,
    cpu: f32,
    mem_mb: f64,
}

#[derive(Serialize)]
struct PerfSample {
    cpu_total: f32,
    mem_mb_total: f64,
    procs: Vec<ProcInfo>,
}

/// Whether the overlay is enabled for this run (env `TIDALUNAR_PERF`).
pub fn enabled() -> bool {
    std::env::var_os("TIDALUNAR_PERF").is_some()
}

/// Spawn the sampler thread when the perf flag is set; no-op otherwise.
pub fn start() {
    if !enabled() {
        return;
    }
    let _ = thread::Builder::new()
        .name("perf-sampler".into())
        .spawn(run);
}

fn run() {
    // All CEF subprocesses are spawned from this same binary, so matching by
    // process name collects the browser, renderer, GPU and utility processes
    // while ignoring unrelated ones. Matching the exe *path* is unreliable:
    // current_exe() and sysinfo can report the same binary with different path
    // representations (casing, \\?\ prefix), and exe() may be None without
    // elevated permissions, which would drop every process.
    let Some(self_name) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_owned()))
    else {
        return;
    };
    // sysinfo's cpu_usage() is a percentage of a single core (can exceed 100 on
    // multi-core); divide by the logical core count to get a share of the whole
    // machine, matching what Task Manager reports.
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get() as f32)
        .unwrap_or(1.0);
    let mut sys = System::new();
    loop {
        // with_cmd is required to read each process's `--type=` switch for
        // classify(); the default refresh skips it, which left every CEF
        // subprocess mislabelled as the browser process.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::OnlyIfNotSet),
        );

        let mut sample = PerfSample {
            cpu_total: 0.0,
            mem_mb_total: 0.0,
            procs: Vec::new(),
        };
        for (pid, p) in sys.processes() {
            if p.name() != self_name.as_os_str() {
                continue;
            }
            let cpu = p.cpu_usage() / cpu_count;
            let mem_mb = p.memory() as f64 / (1024.0 * 1024.0);
            sample.cpu_total += cpu;
            sample.mem_mb_total += mem_mb;
            sample.procs.push(ProcInfo {
                kind: classify(p),
                pid: pid.as_u32(),
                cpu,
                mem_mb,
            });
        }
        sample.procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));

        post_emit_with_data(PERF_CHANNEL, &sample);
        // Engine metrics (listeners, style-recalc rate) come from CDP on the UI
        // thread; request them on the same cadence.
        crate::debug::perf_observer::tick();
        thread::sleep(SAMPLE_INTERVAL);
    }
}

fn classify(p: &sysinfo::Process) -> String {
    for arg in p.cmd() {
        if let Some(kind) = arg.to_str().and_then(|s| s.strip_prefix("--type=")) {
            return kind.to_string();
        }
    }
    "browser".to_string()
}
