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
//!
//! `tap_saga_put` reaches what no export carries either, the one path a saga's `put` takes
//! to the store; `route_volume_restore`, on the entry chunk, hands the page-load choice between
//! TIDAL's saved volume and Rust's level to the early runtime. Both are no-ops on a miss too.

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

/// Names the set of rewrites this binary applies to the assets TIDAL's service worker precaches.
///
/// Workbox refetches a precached chunk only when TIDAL bumps its revision, and until then the
/// chunk keeps the shape its fetching binary gave it. `cache_heal.js` drops the worker once when
/// this differs from the epoch the cache was filled under. Hashed from the rewriting sources
/// below, it moves with every edit to them (a comment-only one costs a single refetch).
pub(crate) const REWRITE_EPOCH: u64 = fnv1a64(&[
    include_str!("module_capture.rs"),
    include_str!("csp_filter.rs"),
]);

/// FNV-1a over the parts in order, as if they were one string.
const fn fnv1a64(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut p = 0;
    while p < parts.len() {
        let bytes = parts[p].as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
        p += 1;
    }
    hash
}

/// What rewrite a TIDAL chunk needs on its way to the renderer.
#[derive(Clone, Copy)]
pub(crate) enum ChunkRewrite {
    /// React-family chunk: append a capture of its exports under this module id.
    Capture(&'static str),
    /// Entry chunk: tag the `createRoot` assignment, which no export carries, and route the
    /// saved-volume restore through the early runtime.
    Entry,
    /// Vendored redux-saga: route its PUT runner through the host's tap.
    TapSagaPut,
}

/// Classify a TIDAL asset URL, or `None` when the chunk is left untouched. The
/// React-family names are tried first; `index-` is Vite's entry chunk, the only
/// one that assigns `createRoot`, and `store-` the one that vendors redux-saga.
pub(crate) fn chunk_rewrite(url: &RequestUrl) -> Option<ChunkRewrite> {
    if let Some(id) = target_module_id(url) {
        return Some(ChunkRewrite::Capture(id));
    }
    let name = asset_name(url)?;
    if name.starts_with("index-") {
        Some(ChunkRewrite::Entry)
    } else if name.starts_with("store-") {
        Some(ChunkRewrite::TapSagaPut)
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
    // `.mjs` forces module mode for `export`/`import` to parse (chunks are ESM).
    let Ok(source_type) = oxc::span::SourceType::from_path(Path::new("chunk.mjs")) else {
        return js.to_string();
    };
    let parsed = oxc::parser::Parser::new(&allocator, js, source_type).parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
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

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Start of the identifier ending at `end` (`end` itself when there is none).
fn ident_start(bytes: &[u8], end: usize) -> usize {
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    start
}

/// End of the identifier starting at `start` (`start` itself when there is none).
fn ident_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    end
}

/// Route redux-saga's PUT effect through `globalThis.__lunaSagaPut` (why: `installSagaPutTap`,
/// exposeTidalInternals.ts).
///
/// The runner is library code, `(r?r.put:e.dispatch)(i)`: the minifier renames its locals but
/// not `.put` or `.dispatch`. Only the store arm is wrapped (a channel put is no store action),
/// called unbound as the pick called it. The first match is taken; `js` comes back unchanged
/// when there is none, and the host warns once no put ever arrived.
pub(crate) fn tap_saga_put(js: &str) -> String {
    const NEEDLE: &str = ".put:";
    const CALL: &str = ".dispatch)(";
    let bytes = js.as_bytes();
    let mut from = 0;
    while let Some(rel) = js[from..].find(NEEDLE) {
        let put = from + rel;
        from = put + NEEDLE.len();
        // `(` channel `?` channel `.put:`, both channels the same name.
        let chan_start = ident_start(bytes, put);
        if chan_start == put || chan_start < 2 || bytes[chan_start - 1] != b'?' {
            continue;
        }
        let test_end = chan_start - 1;
        let test_start = ident_start(bytes, test_end);
        if test_start == test_end || test_start == 0 || bytes[test_start - 1] != b'(' {
            continue;
        }
        if bytes[test_start..test_end] != bytes[chan_start..put] {
            continue;
        }
        // `env` `.dispatch)(` action `)`.
        let env_end = ident_end(bytes, from);
        if env_end == from || !js[env_end..].starts_with(CALL) {
            continue;
        }
        let action_start = env_end + CALL.len();
        let action_end = ident_end(bytes, action_start);
        if action_end == action_start || bytes.get(action_end) != Some(&b')') {
            continue;
        }
        let chan = &js[chan_start..put];
        let env = &js[from..env_end];
        let action = &js[action_start..action_end];
        let tapped = format!(
            "({chan}?{chan}.put:(__lunaPutAction=>globalThis.__lunaSagaPut\
             ?globalThis.__lunaSagaPut(__lunaPutAction,{env}.dispatch)\
             :(0,{env}.dispatch)(__lunaPutAction)))({action})"
        );
        let mut out = String::with_capacity(js.len() + tapped.len());
        out.push_str(&js[..test_start - 1]);
        out.push_str(&tapped);
        out.push_str(&js[action_end + 1..]);
        return out;
    }
    js.to_string()
}

/// Route TIDAL's page-load volume restore through `globalThis.__lunaRestoreVolume`.
///
/// After `take(BOOTSTRAP_DONE)` one saga reads the account's saved level
/// (`let e=yield call(read);if(!e)return;`) and puts back `e.mute`, `e.volume`, `e.volumeUnmute`
/// where they differ from the store, which would undo the one level this install plays. The read
/// is wrapped for the slot in `volume_restore.js`, which answers `null` once a level was chosen;
/// the saga's own guard reads that as nothing saved. The match keys on the action type and the
/// three fields, which the minifier keeps (the sibling saga that takes the same action first reads
/// no `volumeUnmute`). With no slot the read passes through; `js` comes back unchanged on no match.
pub(crate) fn route_volume_restore(js: &str) -> String {
    const ACTION: &str = "BOOTSTRAP_DONE";
    const TAKE_END: &str = ");let ";
    const YIELD: &str = "=yield ";
    const GUARD_END: &str = ")return;";
    let bytes = js.as_bytes();
    let mut from = 0;
    while let Some(rel) = js[from..].find(ACTION) {
        let action = from + rel;
        from = action + ACTION.len();
        // The quote closing the action type, whichever one the minifier chose.
        if !matches!(bytes.get(from), Some(b'`' | b'"' | b'\''))
            || !js[from + 1..].starts_with(TAKE_END)
        {
            continue;
        }
        let saved_start = from + 1 + TAKE_END.len();
        let saved_end = ident_end(bytes, saved_start);
        if saved_end == saved_start || !js[saved_end..].starts_with(YIELD) {
            continue;
        }
        let call_start = saved_end + YIELD.len();
        let call_end = ident_end(bytes, call_start);
        if call_end == call_start || bytes.get(call_end) != Some(&b'(') {
            continue;
        }
        let read_start = call_end + 1;
        let read_end = ident_end(bytes, read_start);
        if read_end == read_start || bytes.get(read_end) != Some(&b')') {
            continue;
        }
        let saved = &js[saved_start..saved_end];
        let guard = format!(";if(!{saved}{GUARD_END}");
        if !js[read_end + 1..].starts_with(&guard) {
            continue;
        }
        // The rest of this saga, up to the next generator: the volume restore reads all three.
        let body_start = read_end + 1 + guard.len();
        let body_end = js[body_start..]
            .find("function*")
            .map_or(js.len(), |rel| body_start + rel);
        let body = &js[body_start..body_end];
        // A field, not the head of a longer one (`volume` in `volumeUnmute`) nor the tail of
        // another name ending in the saved one.
        let reads = |field: &str| {
            let needle = format!("{saved}.{field}");
            body.match_indices(&needle).any(|(at, _)| {
                let before = at.checked_sub(1).and_then(|i| body.as_bytes().get(i));
                let after = body.as_bytes().get(at + needle.len());
                !before.is_some_and(|b| is_ident_byte(*b))
                    && !after.is_some_and(|b| is_ident_byte(*b))
            })
        };
        if !(reads("mute") && reads("volume") && reads("volumeUnmute")) {
            continue;
        }
        let read = &js[read_start..read_end];
        let routed =
            format!("()=>(globalThis.__lunaRestoreVolume||(__lunaSaved=>__lunaSaved))({read}())");
        let mut out = String::with_capacity(js.len() + routed.len());
        out.push_str(&js[..read_start]);
        out.push_str(&routed);
        out.push_str(&js[read_end..]);
        return out;
    }
    js.to_string()
}

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
            // The entry chunk is megabytes and a captured chunk kilobytes, the redux-saga one in
            // between. This only sizes the first allocation: the buffer itself grows unbounded.
            let capacity = match rewrite {
                ChunkRewrite::Capture(_) => 128 * 1024,
                ChunkRewrite::Entry => 2 * 1024 * 1024,
                ChunkRewrite::TapSagaPut => 256 * 1024,
            };
            Some(new_buffering_filter(
                capacity,
                Arc::new(move |body| {
                    FilterOutcome::Emit(match std::str::from_utf8(&body) {
                        Ok(js) => match rewrite {
                            ChunkRewrite::Capture(id) => append_capture(js, id).into_bytes(),
                            ChunkRewrite::Entry => {
                                route_volume_restore(&tag_create_root(js)).into_bytes()
                            }
                            ChunkRewrite::TapSagaPut => tap_saga_put(js).into_bytes(),
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
