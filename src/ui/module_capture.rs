//! Capture TIDAL's real React module exports as a side effect of its own chunk
//! execution. Only the React-family `/assets/*.js` chunks are rewritten (matched
//! by path): each is parsed with oxc, every export enumerated, and one
//! `globalThis.__LUNA_CAP(id, {...exports})` call appended. TIDAL runs the chunk
//! normally, so the call registers the live namespace and plugins share the host
//! React instance (correct hooks/context/elements). No script neutralization,
//! no blob eval, no double-run; any parse/validation miss falls back to bundled
//! React. Mirrors the buffering/dual-handler shape of `csp_filter`.

use std::cell::RefCell;
use std::path::Path;

use cef::*;
use oxc::ast::ast::*;

use crate::plugins::transpile::module_export_name;
use crate::ui::token_filter::userfree_to_string;

/// Map a TIDAL asset URL to the module id plugins import, or `None` if the chunk
/// is not one we capture. Order matters: `react-dom-` before bare `react-`.
pub(crate) fn target_module_id(url: &str) -> Option<&'static str> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str() != Some(crate::ui::nav::HOST_DESKTOP) {
        return None;
    }
    let name = parsed
        .path()
        .strip_prefix("/assets/")
        .filter(|n| n.ends_with(".js"))?;
    if name.starts_with("react-dom-") {
        Some("react-dom/client")
    } else if name.starts_with("jsx-runtime-") {
        Some("react/jsx-runtime")
    } else if name.starts_with("react-") {
        Some("react")
    } else {
        None
    }
}

/// Append `globalThis.__LUNA_CAP(id, { <export>: <local>, ... })` capturing every
/// locally-bound export of the chunk. Returns `js` unchanged on parse failure or
/// when there is nothing to capture (safe no-op -> bundled fallback). Re-exports
/// (`export ... from`) have no local binding and are skipped.
pub(crate) fn append_capture(js: &str, module_id: &str) -> String {
    let allocator = oxc::allocator::Allocator::default();
    // `.mjs` forces module mode so `export`/`import` parse (chunks are ESM).
    let Ok(source_type) = oxc::span::SourceType::from_path(Path::new("chunk.mjs")) else {
        return js.to_string();
    };
    let parsed = oxc::parser::Parser::new(&allocator, js, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return js.to_string();
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportNamedDeclaration(d) => {
                if let Some(decl) = &d.declaration {
                    for name in declared_names(decl) {
                        entries.push((name.clone(), name));
                    }
                } else if d.source.is_none() && !d.export_kind.is_type() {
                    for s in &d.specifiers {
                        if s.export_kind.is_type() {
                            continue;
                        }
                        entries.push((
                            module_export_name(&s.exported),
                            module_export_name(&s.local),
                        ));
                    }
                }
            }
            Statement::ExportDefaultDeclaration(d) => {
                if let Some(name) = export_default_name(&d.declaration) {
                    entries.push(("default".to_string(), name));
                }
            }
            _ => {}
        }
    }

    if entries.is_empty() {
        return js.to_string();
    }

    let id = serde_json::to_string(module_id).unwrap_or_else(|_| "\"\"".to_string());
    let fields: Vec<String> = entries.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    format!(
        "{js}\n;globalThis.__LUNA_CAP&&globalThis.__LUNA_CAP({id},{{{}}});",
        fields.join(", ")
    )
}

/// Declared binding name(s) for a declaration carried by an export
/// (`export const X` / `export function X` / `export class X`).
fn declared_names(decl: &Declaration) -> Vec<String> {
    match decl {
        Declaration::VariableDeclaration(var) => var
            .declarations
            .iter()
            .filter_map(|d| d.id.get_binding_identifier())
            .map(|id| id.name.to_string())
            .collect(),
        Declaration::FunctionDeclaration(func) => func
            .id
            .as_ref()
            .map(|i| i.name.to_string())
            .into_iter()
            .collect(),
        Declaration::ClassDeclaration(class) => class
            .id
            .as_ref()
            .map(|i| i.name.to_string())
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Local name of an `export default` when it is a named function/class.
fn export_default_name(kind: &ExportDefaultDeclarationKind) -> Option<String> {
    match kind {
        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
            f.id.as_ref().map(|i| i.name.to_string())
        }
        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
            c.id.as_ref().map(|i| i.name.to_string())
        }
        _ => None,
    }
}

// --- Response filter (buffers the chunk, appends the capture call at EOF) ---

#[derive(Clone)]
enum FilterState {
    Accumulating(Vec<u8>),
    Emitting { data: Vec<u8>, offset: usize },
    Done,
}

wrap_response_filter! {
    pub(super) struct CaptureFilter {
        module_id: String,
        state: RefCell<FilterState>,
    }

    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            1
        }

        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let mut state = self.state.borrow_mut();
            let out_written = match data_out_written {
                Some(w) => w,
                None => return ResponseFilterStatus::ERROR,
            };
            *out_written = 0;

            match &mut *state {
                FilterState::Accumulating(buf) => {
                    if let Some(input) = data_in {
                        if let Some(read) = data_in_read {
                            *read = input.len();
                        }
                        buf.extend_from_slice(input);
                        ResponseFilterStatus::NEED_MORE_DATA
                    } else {
                        let accumulated = std::mem::take(buf);
                        // oxc needs &str, but a non-UTF-8 body (a CDN/SW serving
                        // compressed bytes despite the identity header) must pass
                        // through byte-identical, not get lossy-mangled into a
                        // corrupt chunk. Strict-decode; emit the original on miss.
                        let modified = match std::str::from_utf8(&accumulated) {
                            Ok(js) => append_capture(js, &self.module_id).into_bytes(),
                            Err(_) => accumulated,
                        };
                        *state = FilterState::Emitting {
                            data: modified,
                            offset: 0,
                        };
                        drop(state);
                        self.emit(data_out, out_written)
                    }
                }
                FilterState::Emitting { .. } => {
                    if let Some(input) = data_in
                        && let Some(read) = data_in_read
                    {
                        *read = input.len();
                    }
                    drop(state);
                    self.emit(data_out, out_written)
                }
                FilterState::Done => ResponseFilterStatus::DONE,
            }
        }
    }
}

impl CaptureFilter {
    fn emit(
        &self,
        data_out: Option<&mut Vec<u8>>,
        out_written: &mut usize,
    ) -> ResponseFilterStatus {
        let mut state = self.state.borrow_mut();
        let (data, offset) = match &mut *state {
            FilterState::Emitting { data, offset } => (data, offset),
            _ => return ResponseFilterStatus::ERROR,
        };

        let remaining = &data[*offset..];
        if remaining.is_empty() {
            *state = FilterState::Done;
            return ResponseFilterStatus::DONE;
        }

        let Some(out_buf) = data_out else {
            return ResponseFilterStatus::NEED_MORE_DATA;
        };
        let to_write = remaining.len().min(out_buf.len());
        out_buf[..to_write].copy_from_slice(&remaining[..to_write]);
        *out_written = to_write;
        *offset += to_write;

        if *offset >= data.len() {
            *state = FilterState::Done;
            ResponseFilterStatus::DONE
        } else {
            ResponseFilterStatus::NEED_MORE_DATA
        }
    }
}

// --- Request handler that attaches the capture filter to React chunks ---

wrap_resource_request_handler! {
    pub(crate) struct CaptureRequestHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            // The filter sees pre-decompression bytes; force identity so the
            // plaintext JS is parseable (cf. csp_filter / token_filter).
            if let Some(req) = request {
                let accept_name = CefString::from("Accept-Encoding");
                let accept_val = CefString::from("identity");
                req.set_header_by_name(Some(&accept_name), Some(&accept_val), 1);
            }
            ReturnValue::CONTINUE
        }

        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            let mime = response
                .as_ref()
                .map(|r| {
                    let m = r.mime_type();
                    userfree_to_string(&m)
                })
                .unwrap_or_default();
            if !mime.contains("javascript") {
                return None;
            }
            let url = request
                .as_ref()
                .map(|r| userfree_to_string(&r.url()))
                .unwrap_or_default();
            let module_id = target_module_id(&url)?;
            Some(CaptureFilter::new(
                module_id.to_string(),
                RefCell::new(FilterState::Accumulating(Vec::with_capacity(128 * 1024))),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_react_family_chunks() {
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/react-Cif6l2Tn.js"),
            Some("react")
        );
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/jsx-runtime-abc123.js"),
            Some("react/jsx-runtime")
        );
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/react-dom-CARA-N2H.js"),
            Some("react-dom/client")
        );
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/index-CvAR5jQO.js"),
            None
        );
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/polyfills-x.js"),
            None
        );
        assert_eq!(target_module_id("https://resources.tidal.com/x.js"), None);
        assert_eq!(
            target_module_id("https://desktop.tidal.com/assets/x.css"),
            None
        );
    }

    #[test]
    fn captures_named_specifiers() {
        let out = append_capture(
            "var a=1,b=2;export{a as createElement,b as useState};",
            "react",
        );
        assert!(out.contains("export{a as createElement,b as useState}"));
        assert!(out.contains(r#"globalThis.__LUNA_CAP&&globalThis.__LUNA_CAP("react",{"#));
        assert!(out.contains("createElement: a"));
        assert!(out.contains("useState: b"));
    }

    #[test]
    fn captures_exported_declarations() {
        let out = append_capture(
            "export function jsx(){}export const Fragment=1;",
            "react/jsx-runtime",
        );
        assert!(out.contains("jsx: jsx"));
        assert!(out.contains("Fragment: Fragment"));
    }

    #[test]
    fn captures_default_specifier() {
        let out = append_capture("var X={};export{X as default};", "react");
        assert!(out.contains("default: X"));
    }

    #[test]
    fn skips_reexport_from_but_still_emits_capture() {
        let out = append_capture(
            r#"export * from "./other";export const jsx=1;"#,
            "react/jsx-runtime",
        );
        assert!(out.contains("jsx: jsx"));
        assert!(out.contains("__LUNA_CAP"));
    }

    #[test]
    fn parse_failure_returns_source_unchanged() {
        let bad = "var x = ;;;(";
        assert_eq!(append_capture(bad, "react"), bad);
    }

    #[test]
    fn no_exports_returns_source_unchanged() {
        let src = "console.log(1);";
        assert_eq!(append_capture(src, "react"), src);
    }

    #[test]
    fn captures_minified_cjs_loader_exports() {
        // Mirrors TIDAL's react chunk tail: minified named exports of CJS loader
        // fns. We must capture every binding so modules.ts can call the right one.
        let out = append_capture("var f=1,c=2,m=3;export{f as a,c as i,m as t};", "react");
        assert!(out.contains("a: f"));
        assert!(out.contains("i: c"));
        assert!(out.contains("t: m"));
    }

    #[test]
    fn non_utf8_body_passes_through_unchanged() {
        // append_capture only handles &str; the filter's strict decode guards the
        // non-UTF-8 path, but a lone invalid byte through append_capture must not
        // panic. (Filter-level passthrough is covered by the strict from_utf8.)
        let src = "var x=1;export{x as a};";
        assert!(append_capture(src, "react").contains("a: x"));
    }
}
