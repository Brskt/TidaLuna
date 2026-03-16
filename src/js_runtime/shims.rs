use rquickjs::{Ctx, Function, Object, Result as JsResult};

/// Install all globalThis shims into the given JS context.
pub fn install_shims(ctx: &Ctx<'_>) -> JsResult<()> {
    install_window(ctx)?;
    install_timers(ctx)?;
    install_base64(ctx)?;
    install_performance(ctx)?;
    install_text_codec(ctx)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Window shim: globalThis.window = globalThis + no-op event listeners
// ---------------------------------------------------------------------------
// Many JS libraries reference `window` — this prevents ReferenceError.
// addEventListener/removeEventListener are no-ops since QuickJS has no DOM.

fn install_window(ctx: &Ctx<'_>) -> JsResult<()> {
    ctx.eval::<(), _>(r#"
        globalThis.window = globalThis;
        const stubEl = () => ({
            style: {}, appendChild: () => stubEl(), removeChild: () => {},
            innerHTML: "", innerText: "", id: "", className: "",
            setAttribute: () => {}, getAttribute: () => null,
            addEventListener: () => {}, removeEventListener: () => {},
            querySelector: () => null, querySelectorAll: () => [],
            children: [], childNodes: [], parentNode: null,
            sheet: { insertRule: () => 0, deleteRule: () => {}, cssRules: [] },
        });
        const docStub = {
            createElement: () => stubEl(),
            createTextNode: () => stubEl(),
            getElementById: () => null,
            querySelector: () => null,
            querySelectorAll: () => [],
            body: stubEl(),
            head: stubEl(),
            documentElement: stubEl(),
        };
        globalThis.document = docStub;
        globalThis.addEventListener = function() {};
        globalThis.removeEventListener = function() {};
        globalThis.navigator = { userAgent: "QuickJS/TidaLunar" };
        globalThis.MutationObserver = class MutationObserver {
            constructor(_cb) {}
            observe() {}
            disconnect() {}
            takeRecords() { return []; }
        };
    "#)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Timers: setTimeout, setInterval, clearTimeout, clearInterval
// ---------------------------------------------------------------------------
//
// Implemented entirely in JS — callbacks + deadlines stored in a hidden map.
// Rust provides __now_ms() for monotonic time and drive_timers() to tick.

use std::time::Instant;

thread_local! {
    static TIMER_EPOCH: Instant = Instant::now();
}

fn install_timers(ctx: &Ctx<'_>) -> JsResult<()> {
    // Rust-backed monotonic clock
    ctx.globals().set(
        "__now_ms",
        Function::new(ctx.clone(), || -> f64 {
            TIMER_EPOCH.with(|epoch| epoch.elapsed().as_secs_f64() * 1000.0)
        })?.with_name("__now_ms")?,
    )?;

    // Timer system in pure JS — avoids rquickjs lifetime issues
    ctx.eval::<(), _>(r#"
        (function() {
            const timers = new Map();
            let nextId = 1;

            globalThis.setTimeout = function(cb, ms) {
                const id = nextId++;
                timers.set(id, { cb, deadline: __now_ms() + (ms || 0), interval: 0 });
                return id;
            };
            globalThis.setInterval = function(cb, ms) {
                const id = nextId++;
                const period = Math.max(ms || 0, 1);
                timers.set(id, { cb, deadline: __now_ms() + period, interval: period });
                return id;
            };
            globalThis.clearTimeout = function(id) { timers.delete(id); };
            globalThis.clearInterval = function(id) { timers.delete(id); };

            // Called by Rust to fire ready timers
            globalThis.__drive_timers = function() {
                const now = __now_ms();
                for (const [id, t] of timers) {
                    if (now >= t.deadline) {
                        try { t.cb(); } catch(e) { console.error("Timer error:", e); }
                        if (t.interval > 0) {
                            t.deadline = now + t.interval;
                        } else {
                            timers.delete(id);
                        }
                    }
                }
            };
        })();
    "#)?;

    Ok(())
}

/// Drive pending timers — call this periodically from the host loop.
pub fn drive_timers(ctx: &Ctx<'_>) -> JsResult<()> {
    ctx.eval::<(), _>("__drive_timers()")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Base64: atob / btoa
// ---------------------------------------------------------------------------

fn install_base64(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();

    globals.set(
        "btoa",
        Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
            use base64::Engine;
            Ok(base64::engine::general_purpose::STANDARD.encode(input.as_bytes()))
        })?.with_name("btoa")?,
    )?;

    globals.set(
        "atob",
        Function::new(ctx.clone(), |input: String| -> rquickjs::Result<String> {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&input)
                .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &format!("atob: {e}")))?;
            String::from_utf8(bytes)
                .map_err(|e| rquickjs::Error::new_from_js_message("string", "string", &format!("atob: {e}")))
        })?.with_name("atob")?,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// performance.now()
// ---------------------------------------------------------------------------

fn install_performance(ctx: &Ctx<'_>) -> JsResult<()> {
    let start = Instant::now();

    let perf = Object::new(ctx.clone())?;
    perf.set(
        "now",
        Function::new(ctx.clone(), move || -> f64 {
            start.elapsed().as_secs_f64() * 1000.0
        })?.with_name("now")?,
    )?;

    ctx.globals().set("performance", perf)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// TextEncoder / TextDecoder (UTF-8 only) — defined in pure JS
// ---------------------------------------------------------------------------

fn install_text_codec(ctx: &Ctx<'_>) -> JsResult<()> {
    ctx.eval::<(), _>(r#"
        globalThis.TextEncoder = function() {
            this.encoding = "utf-8";
            this.encode = function(str) {
                const buf = [];
                for (let i = 0; i < str.length; i++) {
                    let c = str.charCodeAt(i);
                    if (c < 0x80) {
                        buf.push(c);
                    } else if (c < 0x800) {
                        buf.push(0xC0 | (c >> 6), 0x80 | (c & 0x3F));
                    } else if (c < 0xD800 || c >= 0xE000) {
                        buf.push(0xE0 | (c >> 12), 0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F));
                    } else {
                        i++;
                        c = 0x10000 + (((c & 0x3FF) << 10) | (str.charCodeAt(i) & 0x3FF));
                        buf.push(0xF0 | (c >> 18), 0x80 | ((c >> 12) & 0x3F), 0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F));
                    }
                }
                return new Uint8Array(buf);
            };
        };
        globalThis.TextDecoder = function() {
            this.encoding = "utf-8";
            this.decode = function(buf) {
                let str = "";
                let i = 0;
                const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
                while (i < bytes.length) {
                    let c = bytes[i++];
                    if (c < 0x80) {
                        str += String.fromCharCode(c);
                    } else if (c < 0xE0) {
                        str += String.fromCharCode(((c & 0x1F) << 6) | (bytes[i++] & 0x3F));
                    } else if (c < 0xF0) {
                        str += String.fromCharCode(((c & 0x0F) << 12) | ((bytes[i++] & 0x3F) << 6) | (bytes[i++] & 0x3F));
                    } else {
                        let cp = ((c & 0x07) << 18) | ((bytes[i++] & 0x3F) << 12) | ((bytes[i++] & 0x3F) << 6) | (bytes[i++] & 0x3F);
                        cp -= 0x10000;
                        str += String.fromCharCode(0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF));
                    }
                }
                return str;
            };
        };
    "#)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::js_runtime;

    #[test]
    fn test_set_timeout_and_drive() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            super::install_shims(&ctx).unwrap();

            let _: () = ctx.eval(r#"
                globalThis.__fired = false;
                setTimeout(() => { globalThis.__fired = true; }, 0);
            "#).unwrap();

            super::drive_timers(&ctx).unwrap();

            let fired: bool = ctx.eval("globalThis.__fired").unwrap();
            assert!(fired);
        });
    }

    #[test]
    fn test_clear_timeout() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            super::install_shims(&ctx).unwrap();

            let _: () = ctx.eval(r#"
                globalThis.__fired = false;
                let id = setTimeout(() => { globalThis.__fired = true; }, 0);
                clearTimeout(id);
            "#).unwrap();

            super::drive_timers(&ctx).unwrap();

            let fired: bool = ctx.eval("globalThis.__fired").unwrap();
            assert!(!fired);
        });
    }

    #[test]
    fn test_atob_btoa() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            super::install_shims(&ctx).unwrap();

            let result: String = ctx.eval("btoa('Hello, World!')").unwrap();
            assert_eq!(result, "SGVsbG8sIFdvcmxkIQ==");

            let result: String = ctx.eval("atob('SGVsbG8sIFdvcmxkIQ==')").unwrap();
            assert_eq!(result, "Hello, World!");
        });
    }

    #[test]
    fn test_performance_now() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            super::install_shims(&ctx).unwrap();

            let ms: f64 = ctx.eval("performance.now()").unwrap();
            assert!(ms >= 0.0);
        });
    }

    #[test]
    fn test_text_encoder_decoder() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            super::install_shims(&ctx).unwrap();

            let result: String = ctx.eval(r#"
                const encoder = new TextEncoder();
                const decoder = new TextDecoder();
                const encoded = encoder.encode("hello");
                decoder.decode(encoded);
            "#).unwrap();
            assert_eq!(result, "hello");
        });
    }
}
