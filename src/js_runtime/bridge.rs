use std::cell::RefCell;

use rquickjs::{Ctx, Function, Result as JsResult};

// ---------------------------------------------------------------------------
// CEF Bridge — connects the rquickjs plugin runtime to CEF's DOM/Redux
// ---------------------------------------------------------------------------
//
// Architecture:
//
//   Plugin (rquickjs) → __cef_eval(js_code) → Rust → CEF exec_js()
//
//   CEF event → Rust → __dispatch_event(type, payload) → rquickjs callbacks
//
// For synchronous queries (e.g., store.getState()), the plugin runtime
// can't wait for CEF because it's on a different thread.  Instead:
//   - State is periodically synced from CEF → Rust → rquickjs global
//   - Plugins read from the cached state
//
// For async operations (observe, intercept), we use an event system:
//   - Plugins register listeners in rquickjs
//   - CEF forwards events to Rust
//   - Rust dispatches to rquickjs listeners

// Thread-local queue for JS code that needs to be sent to CEF.
// __cef_eval pushes here instead of calling eval_js() directly,
// because eval_js() → with_state() would deadlock the Mutex
// (we're already inside with_state from the IPC handler).
// The caller drains this queue after ctx.with() returns.
thread_local! {
    static CEF_JS_QUEUE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Drain all queued JS code intended for CEF execution.
/// Call this AFTER releasing the AppState Mutex, then pass each
/// string to `eval_js()`.
pub fn drain_cef_js() -> Vec<String> {
    CEF_JS_QUEUE.with(|q| q.borrow_mut().drain(..).collect())
}

/// Install the CEF bridge globals in the rquickjs context.
///
/// Provides:
///   - `__cef_eval(code)` — fire-and-forget JS execution in CEF
///   - `__cef_state` — cached Redux state (updated by Rust periodically)
///   - `__on_cef_event(type, callback)` — register event listener
///   - `__dispatch_cef_event(type, payload_json)` — called by Rust to fire events
pub fn install_bridge(ctx: &Ctx<'_>) -> JsResult<()> {
    // Event listener system in pure JS
    ctx.eval::<(), _>(r#"
        (function() {
            const listeners = {};

            // Register a listener for a CEF event type
            globalThis.__on_cef_event = function(type, callback) {
                if (!listeners[type]) listeners[type] = [];
                listeners[type].push(callback);
                // Return unsubscribe function
                return function() {
                    const arr = listeners[type];
                    if (arr) {
                        const idx = arr.indexOf(callback);
                        if (idx >= 0) arr.splice(idx, 1);
                    }
                };
            };

            // Dispatch an event from Rust (called when CEF sends data back)
            globalThis.__dispatch_cef_event = function(type, payloadJson) {
                const cbs = listeners[type];
                if (!cbs || cbs.length === 0) return;
                let payload;
                try { payload = JSON.parse(payloadJson); } catch { payload = payloadJson; }
                for (const cb of cbs) {
                    try { cb(payload); } catch(e) { console.error("CEF event error:", type, e); }
                }
            };

            // Cached Redux state — updated by Rust periodically
            globalThis.__cef_state = {};

            // Cached Redux intercept listeners
            const interceptors = {};

            // intercept(actionType, callback) — Redux action interception
            globalThis.__intercept = function(actionType, unloads, callback) {
                if (!interceptors[actionType]) interceptors[actionType] = new Set();
                interceptors[actionType].add(callback);
                const unsub = function() { interceptors[actionType]?.delete(callback); };
                if (unloads && typeof unloads.add === "function") unloads.add(unsub);
                return unsub;
            };

            // Called by Rust when CEF dispatches a Redux action
            globalThis.__dispatch_redux_action = function(actionType, payloadJson) {
                const cbs = interceptors[actionType];
                if (!cbs || cbs.size === 0) return;
                let payload;
                try { payload = JSON.parse(payloadJson); } catch { payload = payloadJson; }
                for (const cb of cbs) {
                    try { cb(payload, actionType); } catch(e) { console.error("Intercept error:", actionType, e); }
                }
            };
        })();
    "#)?;

    // __cef_eval — queues JS code for later execution in CEF.
    // Pushed to a thread-local queue (not eval_js directly) to avoid
    // Mutex deadlock. The queue is drained after ctx.with() returns.
    ctx.globals().set(
        "__cef_eval",
        Function::new(ctx.clone(), |code: String| {
            CEF_JS_QUEUE.with(|q| q.borrow_mut().push(code));
        })?.with_name("__cef_eval")?,
    )?;

    Ok(())
}

/// Dispatch a CEF event into the rquickjs context.
/// Called from Rust when CEF sends data (Redux action, DOM event, etc.)
pub fn dispatch_event(ctx: &Ctx<'_>, event_type: &str, payload_json: &str) -> JsResult<()> {
    let escaped_type = event_type.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_payload = payload_json.replace('\\', "\\\\").replace('"', "\\\"");
    let code = format!(
        r#"__dispatch_cef_event("{escaped_type}", "{escaped_payload}")"#
    );
    ctx.eval::<(), _>(code.as_str())
}

/// Dispatch a Redux action intercept into the rquickjs context.
pub fn dispatch_redux_action(ctx: &Ctx<'_>, action_type: &str, payload_json: &str) -> JsResult<()> {
    let escaped_type = action_type.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_payload = payload_json.replace('\\', "\\\\").replace('"', "\\\"");
    let code = format!(
        r#"__dispatch_redux_action("{escaped_type}", "{escaped_payload}")"#
    );
    ctx.eval::<(), _>(code.as_str())
}

/// Update the cached Redux state in rquickjs.
/// Called periodically from Rust with state snapshot from CEF.
pub fn update_redux_state(ctx: &Ctx<'_>, state_json: &str) -> JsResult<()> {
    let code = format!("globalThis.__cef_state = {};", state_json);
    ctx.eval::<(), _>(code.as_str())
}

#[cfg(test)]
mod tests {
    use crate::js_runtime;

    #[test]
    fn test_bridge_event_system() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_bridge(&ctx).unwrap();

            // Register a listener
            let _: () = ctx.eval(r#"
                globalThis.__received = null;
                __on_cef_event("test_event", function(data) {
                    globalThis.__received = data;
                });
            "#).unwrap();

            // Dispatch from Rust
            super::dispatch_event(&ctx, "test_event", r#"{"hello":"world"}"#).unwrap();

            // Check it was received
            let received: String = ctx.eval(r#"JSON.stringify(globalThis.__received)"#).unwrap();
            assert_eq!(received, r#"{"hello":"world"}"#);
        });
    }

    #[test]
    fn test_bridge_unsubscribe() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_bridge(&ctx).unwrap();

            let _: () = ctx.eval(r#"
                globalThis.__count = 0;
                const unsub = __on_cef_event("inc", function() { globalThis.__count++; });
                unsub(); // Immediately unsubscribe
            "#).unwrap();

            super::dispatch_event(&ctx, "inc", "null").unwrap();

            let count: i32 = ctx.eval("globalThis.__count").unwrap();
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn test_redux_intercept() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_bridge(&ctx).unwrap();

            let _: () = ctx.eval(r#"
                globalThis.__intercepted = null;
                __intercept("playbackControls/MEDIA_PRODUCT_TRANSITION", null, function(payload, type) {
                    globalThis.__intercepted = { payload, type };
                });
            "#).unwrap();

            super::dispatch_redux_action(
                &ctx,
                "playbackControls/MEDIA_PRODUCT_TRANSITION",
                r#"{"productId":"12345"}"#,
            ).unwrap();

            let result: String = ctx.eval("JSON.stringify(globalThis.__intercepted)").unwrap();
            assert!(result.contains("12345"));
            assert!(result.contains("MEDIA_PRODUCT_TRANSITION"));
        });
    }

    #[test]
    fn test_cached_redux_state() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_bridge(&ctx).unwrap();

            // Update cached state
            super::update_redux_state(&ctx, r#"{"session":{"userId":42}}"#).unwrap();

            let user_id: i32 = ctx.eval("__cef_state.session.userId").unwrap();
            assert_eq!(user_id, 42);
        });
    }

    #[test]
    fn test_cef_eval_queues_js() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_bridge(&ctx).unwrap();

            // Call __cef_eval from JS — should NOT call eval_js, just queue
            let _: () = ctx.eval(r#"
                __cef_eval("console.log('hello from CEF')");
                __cef_eval("document.title = 'test'");
            "#).unwrap();
        });

        // After ctx.with() returns, drain the queue
        let queued = super::drain_cef_js();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0], "console.log('hello from CEF')");
        assert_eq!(queued[1], "document.title = 'test'");

        // Queue should be empty after drain
        let queued2 = super::drain_cef_js();
        assert!(queued2.is_empty());
    }
}
