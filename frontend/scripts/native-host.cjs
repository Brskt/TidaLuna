// Native plugin host — sandboxed eval with module trust system.
//
// Plugins are evaluated in a wrapper that shadows dangerous globals.
// Only whitelisted modules pass through require() directly.
// Blocked modules throw TRUST_REQUIRED:<module> — Rust parses this
// sentinel, prompts the user, and re-sends the register command
// with the module granted.
//
// This is a JS-level guardrail, not an OS-level sandbox.
// Constructor chaining can bypass the shadows.

const readline = require("readline");
const { createHash } = require("crypto");

// ── Module whitelist (pass through, no trust needed) ────────────────────
const SAFE_MODULES = new Set([
    // Data/utility (no system or network access)
    "assert", "buffer", "console", "constants", "crypto", "domain",
    "events", "path", "punycode", "querystring", "stream",
    "string_decoder", "timers", "tty", "url", "util", "zlib",
    "path/posix", "path/win32", "stream/consumers", "stream/web",
    "util/types", "diagnostics_channel",
    "async_hooks", "perf_hooks", "trace_events", "wasi",
    // Network — allowed freely (same rationale as fetch: no real token in Bun)
    "net", "http", "https", "http2", "tls", "dgram", "dns",
]);

// ── Blocked modules (require explicit user trust) ───────────────────────
// These provide filesystem/subprocess/system access — can read tokens from disk.
const BLOCKED_MODULES = new Set([
    "fs", "fs/promises", "child_process", "worker_threads", "cluster",
    "os", "vm", "v8", "inspector",
]);

function canonicalize(id) {
    return id.startsWith("node:") ? id.slice(5) : id;
}

function isSafe(id) {
    return SAFE_MODULES.has(id) || SAFE_MODULES.has(canonicalize(id));
}

function isBlocked(id) {
    return BLOCKED_MODULES.has(id) || BLOCKED_MODULES.has(canonicalize(id));
}

// ── Mocked process (filtered env, no exit/kill/binding) ─────────────────
const ALLOWED_ENV_KEYS = new Set([
    "TMPDIR", "TMP", "TEMP", "XDG_RUNTIME_DIR",
    "HOME", "USERPROFILE", "PATH", "APPDATA",
]);

const mockedProcess = Object.freeze({
    env: Object.freeze(Object.fromEntries(
        Object.entries(process.env).filter(([k]) => ALLOWED_ENV_KEYS.has(k))
    )),
    platform: process.platform,
    arch: process.arch,
    version: process.version,
    versions: Object.freeze({ ...process.versions }),
    nextTick: process.nextTick.bind(process),
    hrtime: process.hrtime,
    cwd: () => process.cwd(),
    argv: Object.freeze([...process.argv]),
});

// ── Proxied require factory ─────────────────────────────────────────────
function makeRequireProxy(trustedModules) {
    return function proxiedRequire(id) {
        if (isSafe(id)) return require(id);

        if (isBlocked(id)) {
            var canonical = canonicalize(id);
            if (trustedModules.has(id) || trustedModules.has(canonical)) {
                return require(id);
            }
            throw new Error("TRUST_REQUIRED:" + canonical);
        }

        // Relative/absolute paths — blocked
        if (id.startsWith(".") || id.startsWith("/") || /^[a-zA-Z]:/.test(id)) {
            throw new Error("[sandbox] require('" + id + "') blocked: paths not allowed");
        }

        // Unknown third-party — blocked
        throw new Error("[sandbox] require('" + id + "') blocked: not in whitelist");
    };
}

// ── import() pre-check via Bun.Transpiler ───────────────────────────────
var transpiler;
try { transpiler = new Bun.Transpiler({ loader: "js" }); } catch (e) { /* fallback below */ }

function containsDynamicImport(code) {
    if (!transpiler) return true; // can't verify — block (fail-closed)
    try {
        var result = transpiler.scan(code);
        return result.imports.some(function(i) { return i.kind === "dynamic-import"; });
    } catch (e) {
        return true; // unparseable — block (fail-closed)
    }
}

// ── Eval wrapper ────────────────────────────────────────────────────────
// Shadows dangerous globals as parameters set to undefined.
// Uses new Function() because Bun does not support vm.createContext.
// This is the plugin loading mechanism, not arbitrary user input.
// "eval" and "Function" cannot be shadowed as parameters:
// - eval: strict mode forbids it as a parameter name
// - Function: plugins use Function.prototype (fundamental built-in)
// Direct eval(...) and new Function(...) calls remain available.
// This is a known JS-level limitation — same as constructor chaining.
// Web APIs (fetch, WebSocket, etc.) are NOT shadowed — plugins don't
// have the real token, so network access can't exfiltrate it.
// Only Bun-specific APIs and realm-creating primitives are blocked.
const SHADOW_PARAMS = [
    "module", "exports", "require",
    "Bun",
    "Worker", "ShadowRealm",
    "process",
];

// Block globalThis.Bun and globalThis.process so plugins can't bypass
// parameter shadows via property access on the global object.
// Bun.Transpiler is already captured (line 92) — the only Bun API the host uses.
// Capture real process handles before overwriting — IPC loop needs them.
var _realStdin = process.stdin;
var _realStdout = process.stdout;
var _realExit = process.exit.bind(process);
delete globalThis.Bun;
Object.defineProperty(globalThis, 'Bun', {
    value: undefined, writable: false, configurable: false
});
Object.defineProperty(globalThis, 'process', {
    value: mockedProcess, writable: false, configurable: false
});

function evalPlugin(code, proxiedRequire) {
    var m = { exports: {} };
    // eslint-disable-next-line no-new-func -- intentional: plugin code loading
    var fn = new Function(SHADOW_PARAMS.join(","), code); // NOSONAR
    fn(
        m, m.exports, proxiedRequire,
        undefined,
        undefined, undefined,
        mockedProcess
    );
    return m;
}

// ── Hash ────────────────────────────────────────────────────────────────
function hashCode(code) {
    return createHash("sha256").update(code).digest("hex");
}

// ── State ───────────────────────────────────────────────────────────────
const modules = {};

// ── IPC ─────────────────────────────────────────────────────────────────
const rl = readline.createInterface({ input: _realStdin });
rl.on("close", () => _realExit(0));

rl.on("line", async (line) => {
    var cmd;
    try { cmd = JSON.parse(line); } catch { return; }

    var id = cmd.id;
    var type = cmd.type;
    if (!id) return;

    try {
        if (type === "register") {
            var name = cmd.name;
            var code = cmd.code;
            var trust = cmd.trust; // { moduleName: true } grants from Rust

            if (containsDynamicImport(code)) {
                respondError(id, "dynamic import() is not allowed in native plugins");
                return;
            }

            var trustedModules = new Set();
            if (trust) {
                for (var mod in trust) {
                    if (trust[mod] === true) trustedModules.add(mod);
                }
            }

            var proxiedRequire = makeRequireProxy(trustedModules);
            var m = evalPlugin(code, proxiedRequire);
            modules[name] = m.exports;
            respond(id, { ok: true, exports: Object.keys(m.exports), hash: hashCode(code) });

        } else if (type === "call") {
            var name = cmd.name;
            var fnName = cmd.fn;
            var args = cmd.args;
            var mod = modules[name];
            if (!mod) { respondError(id, "module '" + name + "' not registered"); return; }
            var member = mod[fnName];
            if (typeof member === "function") {
                var result = await member(...(args || []));
                respond(id, { ok: true, result: result ?? null });
            } else {
                respond(id, { ok: true, result: member ?? null });
            }

        } else if (type === "cleanup") {
            delete modules[cmd.name];
            respond(id, { ok: true });

        } else {
            respondError(id, "unknown command type: " + type);
        }
    } catch (e) {
        var msg = e?.message || String(e);
        if (e?.stack) msg += "\n" + e.stack;
        respondError(id, msg);
    }
});

function respond(id, data) {
    _realStdout.write(JSON.stringify({ id, ...data }) + "\n");
}

function respondError(id, error) {
    _realStdout.write(JSON.stringify({ id, error }) + "\n");
}
