use rquickjs::{Ctx, Function, Result as JsResult};

/// Install a `fetch()` global that proxies to Rust's reqwest client.
///
/// The fetch is synchronous (blocks the JS thread) since QuickJS is
/// single-threaded anyway.  Returns a Response-like object with:
///   .ok, .status, .statusText, .headers (object), .url
///   .text() → string
///   .json() → parsed object
pub fn install_fetch(ctx: &Ctx<'_>) -> JsResult<()> {
    // Rust-side HTTP request function: __fetch_raw(url, method, headers_json, body)
    // Returns JSON: { status, statusText, ok, url, headers: {}, body: "" }
    ctx.globals().set(
        "__fetch_raw",
        Function::new(ctx.clone(), |url: String, method: String, headers_json: String, body: Option<String>| -> rquickjs::Result<String> {
            // Build and execute HTTP request synchronously using reqwest::blocking
            let result = do_fetch(&url, &method, &headers_json, body.as_deref());
            match result {
                Ok(resp) => Ok(resp),
                Err(e) => Err(rquickjs::Error::new_from_js_message(
                    "fetch",
                    "response",
                    &format!("fetch failed: {e}"),
                )),
            }
        })?.with_name("__fetch_raw")?,
    )?;

    // JS-side fetch() that wraps __fetch_raw into a Response-like object
    ctx.eval::<(), _>(r#"
        globalThis.fetch = function(input, init) {
            const url = typeof input === "string" ? input : input.url;
            const method = (init && init.method) || "GET";
            const headers = (init && init.headers) || {};
            const body = (init && init.body) || undefined;

            // Normalize headers to plain object
            let headersObj = {};
            if (headers && typeof headers === "object") {
                if (typeof headers.entries === "function") {
                    for (const [k, v] of headers.entries()) headersObj[k] = v;
                } else {
                    headersObj = headers;
                }
            }

            const rawJson = __fetch_raw(url, method.toUpperCase(), JSON.stringify(headersObj), body);
            const raw = JSON.parse(rawJson);

            // Build Response-like object
            const response = {
                ok: raw.ok,
                status: raw.status,
                statusText: raw.statusText,
                url: raw.url,
                headers: new __FetchHeaders(raw.headers),
                _body: raw.body,

                text: function() {
                    return Promise.resolve(this._body);
                },
                json: function() {
                    return Promise.resolve(JSON.parse(this._body));
                },
                arrayBuffer: function() {
                    // Basic implementation — encode string to bytes
                    const encoder = new TextEncoder();
                    return Promise.resolve(encoder.encode(this._body).buffer);
                },
                clone: function() {
                    return Object.assign({}, this, {
                        text: this.text,
                        json: this.json,
                        arrayBuffer: this.arrayBuffer,
                        clone: this.clone,
                    });
                },
            };

            return Promise.resolve(response);
        };

        // Minimal Headers implementation
        globalThis.__FetchHeaders = function(obj) {
            this._map = {};
            if (obj) {
                for (const k of Object.keys(obj)) {
                    this._map[k.toLowerCase()] = obj[k];
                }
            }
        };
        __FetchHeaders.prototype.get = function(name) {
            return this._map[name.toLowerCase()] || null;
        };
        __FetchHeaders.prototype.has = function(name) {
            return name.toLowerCase() in this._map;
        };
        __FetchHeaders.prototype.set = function(name, value) {
            this._map[name.toLowerCase()] = value;
        };
        __FetchHeaders.prototype.entries = function() {
            return Object.entries(this._map)[Symbol.iterator]();
        };
        __FetchHeaders.prototype.forEach = function(cb) {
            for (const [k, v] of Object.entries(this._map)) cb(v, k, this);
        };
    "#)?;

    Ok(())
}

/// Execute an HTTP request synchronously using reqwest blocking client.
/// Returns a JSON string with the response data.
fn do_fetch(url: &str, method: &str, headers_json: &str, body: Option<&str>) -> anyhow::Result<String> {
    // Use a blocking client — QuickJS is single-threaded anyway
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut req = match method {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => client.get(url),
    };

    // Parse and apply headers
    if !headers_json.is_empty() && headers_json != "{}" {
        if let Ok(headers) = serde_json::from_str::<std::collections::HashMap<String, String>>(headers_json) {
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
        }
    }

    // Apply body
    if let Some(body) = body {
        req = req.body(body.to_string());
    }

    let resp = req.send()?;

    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();
    let ok = resp.status().is_success();
    let url = resp.url().to_string();

    // Collect response headers
    let mut resp_headers = serde_json::Map::new();
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            resp_headers.insert(k.as_str().to_string(), serde_json::Value::String(v.to_string()));
        }
    }

    let body_text = resp.text().unwrap_or_default();

    let result = serde_json::json!({
        "status": status,
        "statusText": status_text,
        "ok": ok,
        "url": url,
        "headers": resp_headers,
        "body": body_text,
    });

    Ok(result.to_string())
}

#[cfg(test)]
mod tests {
    use crate::js_runtime;

    #[test]
    fn test_fetch_global_exists() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_fetch(&ctx).unwrap();

            let has_fetch: bool = ctx.eval("typeof fetch === 'function'").unwrap();
            assert!(has_fetch);
        });
    }

    #[test]
    fn test_fetch_returns_promise() {
        let (_rt, ctx) = js_runtime::init().expect("init");
        ctx.with(|ctx| {
            js_runtime::shims::install_shims(&ctx).unwrap();
            super::install_fetch(&ctx).unwrap();

            // Fetch a known URL — use httpbin or just verify structure
            // We test with a simple synchronous resolve since our fetch is sync
            let is_promise: bool = ctx.eval(r#"
                const r = fetch("https://httpbin.org/get");
                r instanceof Promise;
            "#).unwrap();
            assert!(is_promise);
        });
    }
}
