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
const realFs = require("fs");
const pathMod = require("path");
const { fileURLToPath } = require("url");

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
function makeRequireProxy(trustedModules, sandboxedFs, dataDir) {
    return function proxiedRequire(id) {
        // Virtual module: plugin data directory
        if (id === "@luna/native-data" || id === "node:@luna/native-data")
            return Object.freeze({ dir: dataDir });

        if (isSafe(id)) return require(id);

        if (isBlocked(id)) {
            var canonical = canonicalize(id);
            // fs/fs-promises: return sandboxed facade instead of real module
            if (canonical === "fs" && sandboxedFs) {
                if (trustedModules.has(id) || trustedModules.has(canonical))
                    return sandboxedFs;
                throw new Error("TRUST_REQUIRED:" + canonical);
            }
            if (canonical === "fs/promises" && sandboxedFs) {
                if (trustedModules.has("fs") || trustedModules.has("fs/promises")
                    || trustedModules.has(id) || trustedModules.has(canonical))
                    return sandboxedFs.promises;
                throw new Error("TRUST_REQUIRED:fs");
            }
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

// globalThis.Bun and globalThis.process are NOT modified — Bun's own built-in
// modules (node:net, node:fs internal streams, node:assert) use Bun.file() and
// process.binding() internally. Nuking these breaks the runtime itself.
// The parameter shadows (Bun=undefined, process=mockedProcess in evalPlugin)
// block the bare identifiers in plugin code. globalThis.Bun and globalThis.process
// remain accessible via property access — documented known limitation.

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

// ── Sandboxed fs ────────────────────────────────────────────────────────
// Minimal fs facade restricted to plugin dataDir + dialog-granted paths.
// Blocks symlink/link/readlink/realpath, delete hors dataDir, options.fd/fs.

function canonicalizeFsPath(p) {
    if (p instanceof URL || (typeof p === 'string' && p.startsWith('file:')))
        p = fileURLToPath(p);
    if (typeof p !== 'string')
        throw new Error("[sandbox] invalid path type");
    var resolved = pathMod.resolve(p);
    try {
        return realFs.realpathSync(resolved);
    } catch (_) {
        return resolveFromExistingAncestor(resolved);
    }
}

function resolveFromExistingAncestor(resolved) {
    var parts = [];
    var current = resolved;
    while (true) {
        try {
            return pathMod.join(realFs.realpathSync(current), ...parts);
        } catch (_) {
            parts.unshift(pathMod.basename(current));
            var parent = pathMod.dirname(current);
            if (parent === current) break;
            current = parent;
        }
    }
    return resolved;
}

function isInDirs(real, dirs) {
    for (var i = 0; i < dirs.length; i++) {
        if (real === dirs[i] || real.startsWith(dirs[i] + pathMod.sep)) return true;
    }
    return false;
}

function assertRead(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return;
    if (grants.readFiles.has(real)) return;
    if (grants.writeFiles.has(real)) return;
    if (isInDirs(real, Array.from(grants.dirs))) return;
    throw new Error("[sandbox] fs read denied: " + p);
}

function assertWrite(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return;
    if (grants.writeFiles.has(real)) return;
    if (isInDirs(real, Array.from(grants.dirs))) return;
    throw new Error("[sandbox] fs write denied: " + p);
}

function assertDelete(p, dataDirs) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return;
    throw new Error("[sandbox] fs delete denied: " + p);
}

function assertMkdir(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return;
    if (isInDirs(real, Array.from(grants.dirs))) return;
    throw new Error("[sandbox] fs mkdir denied: " + p);
}

function rejectUnsafeOpts(opts) {
    if (opts && typeof opts === 'object') {
        if ('fd' in opts) throw new Error("[sandbox] options.fd not allowed");
        if ('fs' in opts) throw new Error("[sandbox] options.fs not allowed");
    }
}

function makeSandboxedFs(dataDirs, grants) {
    function checkRead(p) { assertRead(p, dataDirs, grants); }
    function checkWrite(p) { assertWrite(p, dataDirs, grants); }
    function checkDelete(p) { assertDelete(p, dataDirs); }
    function checkMkdir(p) { assertMkdir(p, dataDirs, grants); }

    var facade = {
        readFileSync: function(p, o) { checkRead(p); return realFs.readFileSync(p, o); },
        writeFileSync: function(p, d, o) { checkWrite(p); return realFs.writeFileSync(p, d, o); },
        existsSync: function(p) { checkRead(p); return realFs.existsSync(p); },
        mkdirSync: function(p, o) { checkMkdir(p); return realFs.mkdirSync(p, o); },
        unlinkSync: function(p) { checkDelete(p); return realFs.unlinkSync(p); },
        rmSync: function(p, o) { checkDelete(p); return realFs.rmSync(p, o); },
        statSync: function(p, o) { checkRead(p); return realFs.statSync(p, o); },
        accessSync: function(p, mode) {
            var m = (mode === undefined) ? realFs.constants.F_OK : mode;
            if (m & realFs.constants.X_OK)
                throw new Error("[sandbox] fs X_OK denied");
            if (m & realFs.constants.W_OK)
                checkWrite(p);
            else
                checkRead(p);
            return realFs.accessSync(p, mode);
        },
        createWriteStream: function(p, o) {
            rejectUnsafeOpts(o);
            checkWrite(p);
            return realFs.createWriteStream(p, o);
        },
        constants: realFs.constants,
    };

    Object.defineProperty(facade, 'promises', {
        get: function() { return makeSandboxedFsPromises(dataDirs, grants); },
        enumerable: true,
    });

    return facade;
}

function makeSandboxedFsPromises(dataDirs, grants) {
    function checkRead(p) { assertRead(p, dataDirs, grants); }
    function checkWrite(p) { assertWrite(p, dataDirs, grants); }
    function checkDelete(p) { assertDelete(p, dataDirs); }
    function checkMkdir(p) { assertMkdir(p, dataDirs, grants); }

    return {
        readFile: function(p, o) { checkRead(p); return realFs.promises.readFile(p, o); },
        writeFile: function(p, d, o) { checkWrite(p); return realFs.promises.writeFile(p, d, o); },
        mkdir: function(p, o) { checkMkdir(p); return realFs.promises.mkdir(p, o); },
        stat: function(p, o) { checkRead(p); return realFs.promises.stat(p, o); },
        unlink: function(p) { checkDelete(p); return realFs.promises.unlink(p); },
        rm: function(p, o) { checkDelete(p); return realFs.promises.rm(p, o); },
        access: function(p, mode) {
            var m = (mode === undefined) ? realFs.constants.F_OK : mode;
            if (m & realFs.constants.X_OK)
                return Promise.reject(new Error("[sandbox] fs X_OK denied"));
            if (m & realFs.constants.W_OK)
                checkWrite(p);
            else
                checkRead(p);
            return realFs.promises.access(p, mode);
        },
    };
}

// ── Per-plugin grant store ──────────────────────────────────────────────
var grantStores = new Map();

function getGrantStore(pluginName) {
    if (!grantStores.has(pluginName))
        grantStores.set(pluginName, { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() });
    return grantStores.get(pluginName);
}

// ── State ───────────────────────────────────────────────────────────────
const modules = {};

// ── IPC ─────────────────────────────────────────────────────────────────
const rl = readline.createInterface({ input: process.stdin });
rl.on("close", () => process.exit(0));

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
            var dataDir = cmd.dataDir;

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

            // Build sandboxed fs restricted to plugin dataDir + grants
            var sandboxedFs = null;
            if (dataDir) {
                realFs.mkdirSync(dataDir, { recursive: true });
                var canonicalDataDir = realFs.realpathSync(dataDir);
                var grants = getGrantStore(name);
                sandboxedFs = makeSandboxedFs([canonicalDataDir], grants);
            }

            var proxiedRequire = makeRequireProxy(trustedModules, sandboxedFs, dataDir);
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

        } else if (type === "grant") {
            if (!cmd.name || !cmd.path || !["read","write","directory"].includes(cmd.mode)) {
                respondError(id, "invalid grant: missing or invalid fields");
                return;
            }
            var grantReal = canonicalizeFsPath(cmd.path);
            var store = getGrantStore(cmd.name);
            if (cmd.mode === "read") store.readFiles.add(grantReal);
            else if (cmd.mode === "write") store.writeFiles.add(grantReal);
            else if (cmd.mode === "directory") store.dirs.add(grantReal);
            respond(id, { ok: true });

        } else if (type === "cleanup") {
            delete modules[cmd.name];
            grantStores.delete(cmd.name);
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
    process.stdout.write(JSON.stringify({ id, ...data }) + "\n");
}

function respondError(id, error) {
    process.stdout.write(JSON.stringify({ id, error }) + "\n");
}
