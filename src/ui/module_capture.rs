//! Capture TIDAL's real React module exports as a side effect of its own chunk
//! execution. The React-family `/assets/*.js` chunks are rewritten (matched by
//! path): each is parsed with oxc, every export enumerated, and one
//! `globalThis.__LUNA_CAP(id, {...exports})` call appended. TIDAL runs the chunk
//! normally: the call registers the live namespace, and plugins share the host
//! React instance (correct hooks/context/elements). No script neutralization,
//! no blob eval, no double-run; any parse/validation miss falls back to bundled
//! React. Mirrors the buffering/dual-handler shape of `csp_filter`.
//!
//! Export capture cannot reach everything. `createRoot` is assigned onto a CJS
//! exports object inside the entry chunk, which exports nothing at all. No import
//! of any kind resolves it. That one gets a second, cheaper rewrite: tagging the
//! assignment itself makes evaluating the chunk bind the host's own root factory
//! to a global (`tag_create_root`). Both rewrites are no-ops when their pattern
//! is absent, and the renderer degrades on its own from there.

use std::path::Path;
use std::sync::Arc;

use cef::*;
use oxc::ast::ast::*;

use crate::plugins::transpile::module_export_name;
use crate::ui::buffering_filter::{FilterOutcome, force_identity_encoding, new_buffering_filter};
use crate::ui::nav::RequestUrl;
use crate::ui::token_filter::userfree_to_string;

/// File name of a TIDAL `/assets/*.js` chunk, or `None` for any other URL.
fn asset_name(url: &RequestUrl) -> Option<&str> {
    let parsed = url.parsed()?;
    if parsed.host_str() != Some(crate::ui::nav::HOST_DESKTOP) {
        return None;
    }
    parsed
        .path()
        .strip_prefix("/assets/")
        .filter(|n| n.ends_with(".js"))
}

/// Map a TIDAL asset URL to the module id plugins import, or `None` if the chunk
/// is not one we capture. Order matters: `react-dom-` before bare `react-`.
pub(crate) fn target_module_id(url: &RequestUrl) -> Option<&'static str> {
    let name = asset_name(url)?;
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

/// What rewrite a TIDAL chunk needs on its way to the renderer.
#[derive(Clone, Copy)]
pub(crate) enum ChunkRewrite {
    /// React-family chunk: append a capture of its exports under this module id.
    Capture(&'static str),
    /// Entry chunk: tag the `createRoot` assignment, which no export carries.
    TagCreateRoot,
}

/// Classify a TIDAL asset URL, or `None` when the chunk is left untouched. The
/// React-family names are tried first; `index-` is Vite's entry chunk, the only
/// one that assigns `createRoot`.
pub(crate) fn chunk_rewrite(url: &RequestUrl) -> Option<ChunkRewrite> {
    if let Some(id) = target_module_id(url) {
        return Some(ChunkRewrite::Capture(id));
    }
    asset_name(url)?
        .starts_with("index-")
        .then_some(ChunkRewrite::TagCreateRoot)
}

/// Append `globalThis.__LUNA_CAP(id, { <export>: <local>, ... })` capturing every
/// locally-bound export of the chunk. Returns `js` unchanged on parse failure or
/// when there is nothing to capture (safe no-op -> bundled fallback). Re-exports
/// (`export ... from`) have no local binding and are skipped.
pub(crate) fn append_capture(js: &str, module_id: &str) -> String {
    let allocator = oxc::allocator::Allocator::default();
    // `.mjs` forces module mode for `export`/`import` to parse (chunks are ESM).
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

/// Bind TIDAL's own `createRoot` to a global by tagging the assignment that
/// defines it: `X.createRoot=` becomes `X.createRoot=globalThis.__lunaCreateRoot=`.
/// Evaluating the chunk then leaves the host's root factory reachable, which no
/// import can do: it sits on a CJS exports object the entry chunk never exports;
/// a renderer that needs a root has no other way to obtain the host's. Only the
/// first assignment is tagged; the other mentions of the name are calls. Returns
/// `js` unchanged when the pattern is absent (a chunk that merely matched the
/// name costs nothing).
pub(crate) fn tag_create_root(js: &str) -> String {
    const NEEDLE: &str = ".createRoot";
    const TAG: &str = "globalThis.__lunaCreateRoot=";
    let bytes = js.as_bytes();
    let mut from = 0;
    while let Some(rel) = js[from..].find(NEEDLE) {
        let start = from + rel;
        from = start + NEEDLE.len();
        // A property of an identifier, not the tail of some longer name.
        let is_property = bytes[..start]
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$');
        if !is_property {
            continue;
        }
        // An assignment, not a call or a comparison: one `=` with no `=` after it.
        let mut eq = from;
        while bytes.get(eq).is_some_and(u8::is_ascii_whitespace) {
            eq += 1;
        }
        if bytes.get(eq) != Some(&b'=') || bytes.get(eq + 1) == Some(&b'=') {
            continue;
        }
        let mut out = String::with_capacity(js.len() + TAG.len());
        out.push_str(&js[..=eq]);
        out.push_str(TAG);
        out.push_str(&js[eq + 1..]);
        return out;
    }
    js.to_string()
}

// --- Request handler that attaches the rewrite filter to TIDAL's chunks ---

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
            // The JS has to be parseable as plaintext.
            if let Some(req) = request {
                force_identity_encoding(req);
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
            let rewrite = chunk_rewrite(&url)?;
            // The entry chunk is megabytes where a React chunk is kilobytes. This
            // only sizes the first allocation: the buffer itself grows unbounded.
            let capacity = match rewrite {
                ChunkRewrite::Capture(_) => 128 * 1024,
                ChunkRewrite::TagCreateRoot => 2 * 1024 * 1024,
            };
            Some(new_buffering_filter(
                capacity,
                Arc::new(move |body| {
                    FilterOutcome::Emit(match std::str::from_utf8(&body) {
                        Ok(js) => match rewrite {
                            ChunkRewrite::Capture(id) => append_capture(js, id).into_bytes(),
                            ChunkRewrite::TagCreateRoot => tag_create_root(js).into_bytes(),
                        },
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
