//! Capture TIDAL's real React exports, for plugins to share, as a side effect of running its own
//! chunks: each chunk's content picks its rewrites (`chunk_rewrites`), and a chunk to capture is
//! oxc-parsed and gets a `globalThis.__LUNA_CAP(id, {...exports})` call per module id.
//!
//! Export capture misses `createRoot`: TIDAL assigns it onto a CJS exports object, and the bundler
//! decides whether the captured react-dom carries it. `tag_create_root` tags that assignment to
//! bind the host's root factory to a global instead. Both rewrites are no-ops on a miss.
//!
//! `tap_saga_put` reaches what no export carries either, the one path a saga's `put` takes
//! to the store; `route_volume_restore` hands the page-load choice between
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

/// Every TIDAL chunk goes through the filter, and its content decides what it gets: TIDAL regroups
/// its modules into differently named chunks between builds (2026.10.14 merged the `react-`,
/// `jsx-runtime-`, `react-dom-` and `store-` ones into `framework-`).
pub(crate) fn is_tidal_chunk(url: &RequestUrl) -> bool {
    asset_name(url).is_some()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkRewrite {
    /// Vendored redux-saga: route its PUT runner through the host's tap.
    TapSagaPut,
    /// Tag the `createRoot` assignment, making the host's root factory reachable whatever the
    /// captured namespaces hold.
    TagCreateRoot,
    RouteVolumeRestore,
    /// Append a capture of the chunk's exports under this module id.
    Capture(&'static str),
}

/// Module id the player SDK's output-devices module is captured under. Its one export is the
/// `outputDevices` singleton the SDK decides device calls from; `frontend/src/output-device.ts`
/// reads it under the same id.
pub(crate) const SDK_OUTPUT_DEVICES: &str = "@tidal-music/player/output-devices";

/// Module id the player SDK's API is captured under: the object a chunk exports that carries `setNext`,
/// `load`, `reset` and the rest under their own names (the namespace TIDAL's entry chunk imports as
/// `sdk`). `frontend/src/sdk-player.ts` reads it under the same id, by that shape, since the minified
/// name of the export changes with every build.
pub(crate) const SDK_PLAYER_API: &str = "@tidal-music/player";

/// The rewrites a chunk's content calls for, splices first and capture last (its parse reads the
/// spliced body). Each signal is a substring the minifier keeps: a React module's assignment onto
/// its CJS exports (`createRoot` only tags, its chunk can export nothing), or an SDK export name.
pub(crate) fn chunk_rewrites(js: &str) -> Vec<ChunkRewrite> {
    let assigns_create_root = js
        .match_indices(".createRoot=")
        .any(|(at, m)| js.as_bytes().get(at + m.len()) != Some(&b'='));
    let mut out = Vec::new();
    if js.contains(SAGA_PUT) && js.contains(SAGA_DISPATCH) {
        out.push(ChunkRewrite::TapSagaPut);
    }
    if assigns_create_root {
        out.push(ChunkRewrite::TagCreateRoot);
    }
    if js.contains(RESTORE_ACTION) {
        out.push(ChunkRewrite::RouteVolumeRestore);
    }
    for (carries, id) in [
        (js.contains(".useState="), "react"),
        (js.contains(".jsxs="), "react/jsx-runtime"),
        (js.contains(".createPortal="), "react-dom/client"),
        (
            js.contains("setNext:()=>") && js.contains("startNativePlayer:()=>"),
            SDK_PLAYER_API,
        ),
        (js.contains(" as outputDevices"), SDK_OUTPUT_DEVICES),
    ] {
        if carries {
            out.push(ChunkRewrite::Capture(id));
        }
    }
    out
}

/// Append `globalThis.__LUNA_CAP(id, { <export>: <local>, ... })` capturing every
/// locally-bound export of the chunk, once per id. Several ids share one namespace
/// object, built once. Returns `js` unchanged on parse failure or
/// when there is nothing to capture (safe no-op -> bundled fallback). Re-exports
/// (`export ... from`) have no local binding and are skipped.
pub(crate) fn append_capture(js: &str, module_ids: &[&str]) -> String {
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

    let id = |m: &str| serde_json::to_string(m).unwrap_or_else(|_| "\"\"".to_string());
    let fields: Vec<String> = entries.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    match module_ids {
        [] => js.to_string(),
        [one] => format!(
            "{js}\n;globalThis.__LUNA_CAP&&globalThis.__LUNA_CAP({},{{{}}});",
            id(one),
            fields.join(", ")
        ),
        many => {
            let calls: Vec<String> = many
                .iter()
                .map(|m| format!("globalThis.__LUNA_CAP({},__lunaNs)", id(m)))
                .collect();
            format!(
                "{js}\n;{{const __lunaNs={{{}}};globalThis.__LUNA_CAP&&({});}}",
                fields.join(", "),
                calls.join(",")
            )
        }
    }
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
/// Evaluating the module then leaves the host's root factory reachable whichever chunk the
/// bundler put it in: `render/src/modules.ts` completes a captured react-dom that lacks
/// `createRoot` from it, and `cache_heal.js` reads it as the sign that root factory ran. Only the
/// first assignment is tagged; the other mentions of the name are calls. Returns
/// `js` unchanged when the pattern is absent (a chunk that merely carries the
/// signal costs nothing).
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

/// The two halves of redux-saga's PUT runner `tap_saga_put` keys on, the signal for it as well.
const SAGA_PUT: &str = ".put:";
const SAGA_DISPATCH: &str = ".dispatch)(";

/// Route redux-saga's PUT effect through `globalThis.__lunaSagaPut` (why: `installSagaPutTap`,
/// exposeTidalInternals.ts).
///
/// The runner is library code, `(r?r.put:e.dispatch)(i)`: the minifier renames its locals but
/// not `.put` or `.dispatch`. Only the store arm is wrapped (a channel put is no store action),
/// called unbound as the pick called it. The first match is taken; `js` comes back unchanged
/// when there is none, and the host warns once no put ever arrived.
pub(crate) fn tap_saga_put(js: &str) -> String {
    let bytes = js.as_bytes();
    let mut from = 0;
    while let Some(rel) = js[from..].find(SAGA_PUT) {
        let put = from + rel;
        from = put + SAGA_PUT.len();
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
        if env_end == from || !js[env_end..].starts_with(SAGA_DISPATCH) {
            continue;
        }
        let action_start = env_end + SAGA_DISPATCH.len();
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

/// The action the volume restore saga takes first, the signal for `route_volume_restore`.
const RESTORE_ACTION: &str = "BOOTSTRAP_DONE";

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
    const TAKE_END: &str = ");let ";
    const YIELD: &str = "=yield ";
    const GUARD_END: &str = ")return;";
    let bytes = js.as_bytes();
    let mut from = 0;
    while let Some(rel) = js[from..].find(RESTORE_ACTION) {
        let action = from + rel;
        from = action + RESTORE_ACTION.len();
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

#[derive(Debug)]
pub(crate) struct RewrittenChunk {
    pub(crate) js: String,
    /// The rewrites whose signal was there but whose pattern was not: TIDAL changed the code
    /// around a signal, and that rewrite left the chunk as it came.
    pub(crate) inert: Vec<ChunkRewrite>,
}

/// The filter below calls nothing else: what it does to a chunk is testable without a CEF
/// response. `None` for a chunk that carries no signal, which goes through as it came.
pub(crate) fn rewrite_chunk(js: &str) -> Option<RewrittenChunk> {
    let rewrites = chunk_rewrites(js);
    if rewrites.is_empty() {
        return None;
    }
    let mut body = js.to_string();
    let mut inert = Vec::new();
    let mut ids = Vec::new();
    for rewrite in rewrites {
        let splice: fn(&str) -> String = match rewrite {
            ChunkRewrite::TapSagaPut => tap_saga_put,
            ChunkRewrite::TagCreateRoot => tag_create_root,
            ChunkRewrite::RouteVolumeRestore => route_volume_restore,
            ChunkRewrite::Capture(id) => {
                ids.push(id);
                continue;
            }
        };
        let spliced = splice(&body);
        if spliced == body {
            inert.push(rewrite);
        }
        body = spliced;
    }
    if !ids.is_empty() {
        let captured = append_capture(&body, &ids);
        if captured == body {
            inert.extend(ids.into_iter().map(ChunkRewrite::Capture));
        }
        body = captured;
    }
    Some(RewrittenChunk { js: body, inert })
}

/// Sizes the first allocation only; the buffer itself grows unbounded.
const CHUNK_CAPACITY: usize = 256 * 1024;

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
            let name = asset_name(&url)?.to_string();
            Some(new_buffering_filter(
                CHUNK_CAPACITY,
                Arc::new(move |body| {
                    let Ok(js) = std::str::from_utf8(&body) else {
                        return FilterOutcome::Emit(body);
                    };
                    let Some(out) = rewrite_chunk(js) else {
                        return FilterOutcome::Emit(body);
                    };
                    for rewrite in &out.inert {
                        crate::verr!(
                            "[CAPTURE] {name}: {rewrite:?} found its signal but not its pattern"
                        );
                    }
                    FilterOutcome::Emit(out.js.into_bytes())
                }),
            ))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/module_capture.rs"]
mod tests;
