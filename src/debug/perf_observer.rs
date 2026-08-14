//! CDP `Performance.getMetrics` bridge for the perf overlay (stage 2).
//!
//! Registers a DevTools message observer on the main browser, enables the
//! Performance domain once, then on each `tick()` requests the metrics. Results
//! arrive asynchronously on the UI thread and are pushed to the page as the
//! `perf.engine` IPC event, where the overlay turns the cumulative counters
//! (`RecalcStyleCount`, `LayoutCount`) into per-second rates. These engine
//! metrics (live listener count, style-recalc rate) are not reachable from page
//! JS, which is the whole reason for going through CDP here.

use std::cell::RefCell;

use cef::*;
use serde::{Deserialize, Serialize};

use crate::app_state::with_state;
use crate::connect::ipc::post_emit_with_data;

const METHOD_ENABLE: i32 = 1;
const METHOD_GET_METRICS: i32 = 2;

thread_local! {
    // Held on the UI thread for the browser's lifetime; `Some` also marks that
    // the observer and `Performance.enable` were already set up: the costly
    // registration happens exactly once.
    static REGISTRATION: RefCell<Option<Registration>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
struct Metric {
    name: String,
    value: f64,
}

#[derive(Deserialize)]
struct GetMetricsResult {
    #[serde(default)]
    metrics: Vec<Metric>,
}

#[derive(Serialize, Default)]
struct EngineMetrics {
    listeners: f64,
    nodes: f64,
    recalc_total: f64,
    layout_total: f64,
}

wrap_dev_tools_message_observer! {
    struct PerfObserver {
        _p: u8,
    }
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            _message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            // CEF assigns its own incrementing message id and ignores the one we
            // pass; results can't be matched by id. Tell a getMetrics reply
            // from the Performance.enable ack by content: the ack carries no
            // metrics array.
            if success == 0 {
                return;
            }
            let Some(bytes) = result else { return };
            let Ok(parsed) = serde_json::from_slice::<GetMetricsResult>(bytes) else {
                return;
            };
            if parsed.metrics.is_empty() {
                return;
            }
            let mut m = EngineMetrics::default();
            for metric in parsed.metrics {
                match metric.name.as_str() {
                    "JSEventListeners" => m.listeners = metric.value,
                    "Nodes" => m.nodes = metric.value,
                    "RecalcStyleCount" => m.recalc_total = metric.value,
                    "LayoutCount" => m.layout_total = metric.value,
                    _ => {}
                }
            }
            post_emit_with_data("perf.engine", &m);
        }
    }
}

wrap_task! {
    struct GetMetricsTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            let Some(browser) = with_state(|s| s.browser.clone()).flatten() else {
                return;
            };
            let Some(host) = browser.host() else { return };

            REGISTRATION.with(|cell| {
                if cell.borrow().is_none() {
                    let mut observer = PerfObserver::new(0);
                    let registration = host.add_dev_tools_message_observer(Some(&mut observer));
                    let _ = host.execute_dev_tools_method(
                        METHOD_ENABLE,
                        Some(&CefString::from("Performance.enable")),
                        None,
                    );
                    *cell.borrow_mut() = registration;
                }
            });

            let _ = host.execute_dev_tools_method(
                METHOD_GET_METRICS,
                Some(&CefString::from("Performance.getMetrics")),
                None,
            );
        }
    }
}

/// Request a fresh `Performance.getMetrics` reading. Callable from any thread;
/// the work is marshalled to the UI thread and the result is delivered later
/// via the observer as the `perf.engine` IPC event.
pub fn tick() {
    let mut task = GetMetricsTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}
