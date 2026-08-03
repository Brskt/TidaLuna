//! Capture TIDAL's real React module exports as a side effect of its own chunk
//! execution. Only the React-family `/assets/*.js` chunks are rewritten (matched
//! by path): each is parsed with oxc, every export enumerated, and one
//! `globalThis.__LUNA_CAP(id, {...exports})` call appended. TIDAL runs the chunk
//! normally, so the call registers the live namespace and plugins share the host
//! React instance (correct hooks/context/elements). No script neutralization,
//! no blob eval, no double-run; any parse/validation miss falls back to bundled
//! React. Mirrors the buffering/dual-handler shape of `csp_filter`.

use std::path::Path;
use std::sync::Arc;

use cef::*;
use oxc::ast::ast::*;

use crate::plugins::transpile::module_export_name;
use crate::ui::buffering_filter::{FilterOutcome, new_buffering_filter};
use crate::ui::nav::RequestUrl;
use crate::ui::token_filter::userfree_to_string;

/// Map a TIDAL asset URL to the module id plugins import, or `None` if the chunk
/// is not one we capture. Order matters: `react-dom-` before bare `react-`.
pub(crate) fn target_module_id(url: &RequestUrl) -> Option<&'static str> {
    let parsed = url.parsed()?;
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
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return js.to_string();
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ExportDeclaration(d) => {
                for name in declared_names(&d.declaration) {
                    entries.push((name.clone(), name));
                }
            }
            // `ExportNamedDeclaration` can no longer carry a source: the old `source.is_none()`
            // test is the variant itself now.
            Statement::ExportNamedDeclaration(d) if !d.export_kind.is_type() => {
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
            let url = RequestUrl::new(
                request
                    .as_ref()
                    .map(|r| userfree_to_string(&r.url()))
                    .unwrap_or_default(),
            );
            let module_id = target_module_id(&url)?.to_string();
            Some(new_buffering_filter(
                128 * 1024,
                Arc::new(move |body| {
                    FilterOutcome::Emit(match std::str::from_utf8(&body) {
                        Ok(js) => append_capture(js, &module_id).into_bytes(),
                        Err(_) => body,
                    })
                }),
            ))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/module_capture.rs"]
mod tests;
