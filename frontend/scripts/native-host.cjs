// Native plugin host — sandboxed with module trust + global hardening.
//
// Plugins are loaded in a wrapper that shadows dangerous globals.
// Only whitelisted modules pass through require() directly.
// Forbidden modules are permanently blocked (no trust possible).
// Blocked modules throw TRUST_REQUIRED:<module> — Rust parses this
// sentinel, prompts the user, and re-sends the register command
// with the module granted.
//
// Global hardening (applied at startup, before any plugin code):
// - globalThis.process replaced with filtering Proxy (no exit/stdio/dlopen)
// - globalThis.Bun replaced with frozen allowlist (no spawn/file/write)
// - globalThis.require/module/exports blocked (prevents proxy bypass)
// - fetch hardened (no file://, no unix sockets)
// - console redirected to stderr (prevents IPC spoofing via stdout)
// - Worker/ShadowRealm/XMLHttpRequest/EventSource/BroadcastChannel blocked
// - AsyncFunction/GeneratorFunction constructors neutered
// - Primordials frozen (prototypes + safe constructors)
// - SAFE_MODULES exports frozen (prevents cross-plugin mutation)
//
// This is a JS-level guardrail, not an OS-level sandbox.
// Function constructor remains available — constructed code lands in
// the same hardened realm with no privileged access.

const readline = require("readline");
const { createHash } = require("crypto");
const realFs = require("fs");
const pathMod = require("path");
const { fileURLToPath } = require("url");

// ── Module whitelist (pass through, no trust needed) ────────────────────
const SAFE_MODULES = new Set([
    // Data/utility (no system or network access)
    "assert", "assert/strict", "buffer", "console", "constants", "crypto",
    "domain", "events", "path", "punycode", "querystring", "stream",
    "string_decoder", "timers", "timers/promises", "url", "util", "zlib",
    "path/posix", "path/win32", "stream/consumers", "stream/web",
    "stream/promises", "util/types", "sys",
    "async_hooks", "perf_hooks",
    // Network — allowed freely (same rationale as fetch: no real token in Bun)
    "net", "http", "https", "http2", "tls", "dns", "dns/promises",
]);

// ── Permanently blocked modules (no trust possible) ─────────────────────
const FORBIDDEN_MODULES = new Set([
    "child_process", "cluster", "vm", "v8",
    "inspector", "inspector/promises", "module", "repl", "process",
    "wasi", "tty", "trace_events",
]);

// ── Blocked modules (require explicit user trust) ───────────────────────
// These provide filesystem/subprocess/system access — can read tokens from disk.
const BLOCKED_MODULES = new Set([
    "fs", "fs/promises", "os", "dgram",
    "diagnostics_channel", "worker_threads",
]);

// ── Pre-load safe modules so Bun internal closures capture refs ────────
// Must happen before any globalThis mutation. Lazy init (bindings, stream
// internals) completes now with mutable prototypes — we freeze later.
SAFE_MODULES.forEach(function(id) {
    try { require(id); } catch (_) {}
});

function canonicalize(id) {
    return id.startsWith("node:") ? id.slice(5) : id;
}

function isSafe(id) {
    return SAFE_MODULES.has(id) || SAFE_MODULES.has(canonicalize(id));
}

function isForbidden(id) {
    return FORBIDDEN_MODULES.has(id) || FORBIDDEN_MODULES.has(canonicalize(id));
}

function isBlocked(id) {
    return BLOCKED_MODULES.has(id) || BLOCKED_MODULES.has(canonicalize(id));
}

// ── Mocked process (filtered env, no exit/kill/binding) ─────────────────
const ALLOWED_ENV_KEYS = new Set([
    "TMPDIR", "TMP", "TEMP",
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

// ── Host-private refs (captured before globalThis hardening) ───────────
// After this point, host code must use these — not the globals.
var hostStdin = process.stdin;
var hostStdout = process.stdout;
var hostStderr = process.stderr;
var hostExit = process.exit.bind(process);
var hostRequire = require;
var _ObjectFreeze = Object.freeze;
var _ObjectDefineProperty = Object.defineProperty;
var _ObjectCreate = Object.create;
var _ObjectKeys = Object.keys;
var _JSONStringify = JSON.stringify;
var _JSONParse = JSON.parse;
var _RealRequest = typeof Request !== "undefined" ? Request : undefined;
var _RealURL = typeof URL !== "undefined" ? URL : undefined;
// Prototype methods — immune to prototype pollution
var _ArrayPrototypeJoin = Array.prototype.join;
var _ArrayPrototypeForEach = Array.prototype.forEach;
var _ArrayPrototypePush = Array.prototype.push;
var _FunctionPrototypeBind = Function.prototype.bind;
var _RealFunction = Function;
var _ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
var _SetPrototypeHas = Set.prototype.has;

// ── Harden globalThis.process via Proxy ───────────────────────────────
// Cannot fully replace — Bun's network modules need process.nextTick etc.
// Proxy blocks dangerous properties while passing safe internals through.
// process.binding() is filtered to only the names http/net/tls need at
// require-time (http_parser, uv). Since SAFE_MODULES are pre-loaded above,
// binding is never actually called again — the filter is defense-in-depth.
;(function hardenProcess() {
    var realProcess = process;
    var blockedKeys = new Set([
        "exit", "abort", "kill",
        "stdin", "stdout", "stderr",
        "mainModule", "getBuiltinModule",
        "execPath", "execArgv",
        "dlopen", "chdir",
        "setuid", "setgid", "seteuid", "setegid",
        "getuid", "getgid", "geteuid", "getegid", "getgroups",
        "_rawDebug", "_linkedBinding", "report",
    ]);
    var allowedBindings = new Set(["http_parser", "uv", "buffer", "constants", "config"]);
    var origBinding = _FunctionPrototypeBind.call(realProcess.binding, realProcess);
    var safeBinding = function(name) {
        if (!_SetPrototypeHas.call(allowedBindings, name))
            throw new Error("[sandbox] process.binding('" + name + "') is not allowed");
        return origBinding(name);
    };
    var filteredEnv = mockedProcess.env; // already frozen + filtered by ALLOWED_ENV_KEYS
    var processProxy = new Proxy(realProcess, {
        get: function(target, prop) {
            if (_SetPrototypeHas.call(blockedKeys, prop))
                throw new Error("[sandbox] process." + prop + " is not available");
            if (prop === "env") return filteredEnv;
            if (prop === "binding") return safeBinding;
            var val = target[prop];
            return typeof val === "function" ? _FunctionPrototypeBind.call(val, target) : val;
        },
        set: function() { throw new Error("[sandbox] process is read-only"); },
        deleteProperty: function() { throw new Error("[sandbox] process is read-only"); },
        defineProperty: function() { throw new Error("[sandbox] process is read-only"); },
    });
    _ObjectDefineProperty(globalThis, "process", {
        value: processProxy, writable: false, configurable: false,
    });
})();

// ── Neutralize console (IPC spoofing prevention) ──────────────────────
// console.log writes to stdout — the same fd as host IPC (JSON lines).
// A plugin could forge IPC responses via console.log. Redirect all
// console output to stderr only.
;(function hardenConsole() {
    var safeConsole = _ObjectCreate(null);
    var noop = function() {};
    var toStderr = function() {
        try {
            hostStderr.write(_ArrayPrototypeJoin.call(arguments, " ") + "\n");
        } catch (_) {}
    };
    _ArrayPrototypeForEach.call(["log", "warn", "error", "info", "debug",
     "trace", "dir", "dirxml", "table", "assert", "time", "timeLog", "timeEnd",
     "count", "countReset"], function(m) { safeConsole[m] = toStderr; });
    _ArrayPrototypeForEach.call(["group", "groupCollapsed", "groupEnd",
     "clear", "profile", "profileEnd", "timeStamp"], function(m) { safeConsole[m] = noop; });
    _ObjectFreeze(safeConsole);
    _ObjectDefineProperty(globalThis, "console", {
        value: safeConsole, writable: false, configurable: false,
    });
})();

// ── Neuter Async/Generator constructors ────────────────────────────────
// Prevents import() bypass via (async function(){}).constructor("return await import('fs')")().
// Function.prototype.constructor is NOT touched — breaks stream/events/util.
;(function neuterAsyncConstructors() {
    var ctors = [
        (async function(){}).constructor,
        (function*(){}).constructor,
        (async function*(){}).constructor,
    ];
    _ArrayPrototypeForEach.call(ctors, function(ctor) {
        _ObjectDefineProperty(ctor.prototype, "constructor", {
            value: undefined, writable: false, configurable: false,
        });
    });
})();

// ── Proxied require factory ─────────────────────────────────────────────
function makeRequireProxy(trustedModules, sandboxedFs, dataDir) {
    return function proxiedRequire(id) {
        // Virtual module: plugin data directory
        if (id === "@luna/native-data" || id === "node:@luna/native-data")
            return _ObjectFreeze({ dir: dataDir });

        // Return hardened console (stderr-only) — real require('console') is stdout-backed
        if (id === "console" || id === "node:console") return globalThis.console;

        if (isSafe(id)) return require(id);

        if (isForbidden(id)) {
            throw new Error("[sandbox] require('" + canonicalize(id) + "') is permanently blocked");
        }

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

// ── Harden eval — scan for dynamic import() before delegating ─────────
// eval("import('fs')") bypasses containsDynamicImport (AST scan of source)
// because the import() is inside a string literal. This wrapper scans the
// final runtime string. Runs on the evaluated string, so concatenation
// like "imp"+"ort('fs')" is caught after assembly.
;(function hardenEval() {
    var realEval = globalThis.eval;
    _ObjectDefineProperty(globalThis, "eval", {
        value: function safeEval(s) {
            if (typeof s === "string" && containsDynamicImport(s))
                throw new Error("[sandbox] eval blocked: dynamic import() is not allowed");
            return realEval(s);
        },
        writable: false, configurable: false,
    });
})();

// ── Harden Function constructor — scan for dynamic import() ───────────
// new Function("return import('fs')")() and (function(){}).constructor("...")
// bypass containsDynamicImport the same way as eval. Replace both
// globalThis.Function and Function.prototype.constructor with a scanning
// wrapper. The host's own new Function() in evalPlugin uses the pre-captured
// SHADOW_PARAMS_STR which is built before this runs.
;(function hardenFunction() {
    var RealFunction = Function;
    var SafeFunction = function() {
        for (var i = 0; i < arguments.length; i++) {
            if (typeof arguments[i] === "string" && containsDynamicImport(arguments[i]))
                throw new Error("[sandbox] Function blocked: dynamic import() is not allowed");
        }
        return RealFunction.apply(this, arguments);
    };
    SafeFunction.prototype = RealFunction.prototype;
    _ObjectDefineProperty(RealFunction.prototype, "constructor", {
        value: SafeFunction, writable: false, configurable: false,
    });
    _ObjectDefineProperty(globalThis, "Function", {
        value: SafeFunction, writable: false, configurable: false,
    });
})();

// ── Harden globalThis.Bun — neuter dangerous methods in-place ─────────
// globalThis.Bun is non-configurable in Bun 1.3.x (ReadOnly|DontDelete),
// so we cannot replace it. But properties ON the Bun object (spawn, file,
// write, etc.) are writable — we neuter them individually.
;(function hardenBun() {
    var realBun = globalThis.Bun;
    if (!realBun) return;
    var DANGEROUS = [
        "spawn", "spawnSync",
        "file", "write",
        "connect", "listen", "serve",
        "openInEditor",
        "Transpiler",
        "stdin", "stdout", "stderr",
        "plugin", "build",
        "$",
        "mmap", "allocUnsafe", "sql", "redis", "s3",
        "env", "embeddedFiles",
    ];
    _ArrayPrototypeForEach.call(DANGEROUS, function(key) {
        if (key in realBun) {
            try {
                _ObjectDefineProperty(realBun, key, {
                    value: undefined, writable: false,
                });
            } catch (_) {
                try { realBun[key] = undefined; } catch (_2) {}
            }
        }
    });
})();

// ── Block globalThis.require/module/exports — prevent proxy bypass ────
;(function hardenGlobalRequire() {
    _ArrayPrototypeForEach.call(["require", "module", "exports", "__dirname", "__filename"], function(prop) {
        _ObjectDefineProperty(globalThis, prop, {
            get: function() { throw new Error("[sandbox] globalThis." + prop + " is not available"); },
            configurable: false,
        });
    });
})();

// ── Harden fetch — block file:// and unix sockets ─────────────────────
;(function hardenFetch() {
    var realFetch = globalThis.fetch;
    if (!realFetch) return;
    function extractUrl(input) {
        if (typeof input === "string") return input;
        if (_RealURL && input instanceof _RealURL) return input.href;
        if (_RealRequest && input instanceof _RealRequest) return input.url;
        return String(input);
    }
    var hardenedFetch = function(input, init) {
        var url = extractUrl(input);
        if (/^file:/i.test(url))
            throw new Error("[sandbox] fetch file:// is not allowed");
        if (init && init.unix)
            throw new Error("[sandbox] fetch unix socket is not allowed");
        // Normalize Request input — decompose into plain URL + init to strip
        // attacker-subclassed Request. Enumerate all standard RequestInit fields.
        if (_RealRequest && input instanceof _RealRequest) {
            var safeInit = {
                method:         input.method,
                headers:        input.headers,
                body:           input.body,
                signal:         input.signal,
                redirect:       input.redirect,
                cache:          input.cache,
                credentials:    input.credentials,
                keepalive:      input.keepalive,
                mode:           input.mode,
                referrer:       input.referrer,
                referrerPolicy: input.referrerPolicy,
                integrity:      input.integrity,
                duplex:         input.duplex,
            };
            // Caller-supplied init wins (mirrors native fetch behaviour)
            if (init) { for (var k in init) safeInit[k] = init[k]; }
            return realFetch.call(globalThis, url, safeInit);
        }
        return realFetch.call(globalThis, input, init);
    };
    _ObjectDefineProperty(globalThis, "fetch", {
        value: hardenedFetch, writable: false, configurable: false,
    });
})();

// ── Block realm creators and dangerous network globals on globalThis ──
;(function hardenGlobals() {
    // Realm creators + WebSocket — no upstream plugin uses the browser WebSocket global
    // (DiscordRPC uses net IPC, ws-based plugins use require('ws') which goes through
    // http/net SAFE_MODULES, not globalThis.WebSocket)
    _ArrayPrototypeForEach.call(["Worker", "ShadowRealm", "WebSocket"], function(prop) {
        _ObjectDefineProperty(globalThis, prop, {
            value: undefined, writable: false, configurable: false,
        });
    });
    // Network globals with no legitimate plugin use
    _ArrayPrototypeForEach.call(["XMLHttpRequest", "EventSource", "BroadcastChannel"], function(prop) {
        if (prop in globalThis) {
            _ObjectDefineProperty(globalThis, prop, {
                value: undefined, writable: false, configurable: false,
            });
        }
    });
})();

// ── Freeze primordials ────────────────────────────────────────────────
// Prevents prototype pollution that could influence host logic.
// Split: full-freeze safe constructors, prototype-only for risky ones.
;(function freezePrimordials() {
    // Safe to fully freeze (no lazy mutation by stdlib after pre-load)
    var fullFreeze = [
        URL, Map, Set, WeakMap, WeakSet, RegExp, Date,
        JSON, Math, Reflect,
        Int8Array, Uint8Array, Int16Array, Uint16Array,
        Int32Array, Uint32Array, Float32Array, Float64Array,
        BigInt64Array, BigUint64Array, Symbol,
    ];
    if (typeof Request !== "undefined") fullFreeze.push(Request);
    if (typeof Headers !== "undefined") fullFreeze.push(Headers);
    if (typeof Response !== "undefined") fullFreeze.push(Response);
    _ArrayPrototypeForEach.call(fullFreeze, function(obj) {
        try { _ObjectFreeze(obj); } catch (_) {}
        try { if (obj.prototype) _ObjectFreeze(obj.prototype); } catch (_) {}
    });

    // Built-in prototypes (Object, Array, Function, String, Promise, Error, etc.)
    // are NOT frozen — npm packages bundled into plugins assign to inherited
    // property names (e.g. node-inspect-extracted), which throws in strict mode
    // when the prototype is frozen. The shared-module mutation vector is covered
    // by freezeSafeModuleExports() below instead.
})();

// ── Freeze SAFE_MODULES exports ───────────────────────────────────────
// Prevents plugins from mutating shared module exports to influence
// host behavior or other plugins.
;(function freezeSafeModuleExports() {
    SAFE_MODULES.forEach(function(id) {
        try {
            var mod = hostRequire(id);
            if (mod && typeof mod === "object") _ObjectFreeze(mod);
        } catch (_) {}
    });
})();

// ── Eval wrapper ────────────────────────────────────────────────────────
// Shadows dangerous globals as parameters set to undefined.
// Parameter shadows are defense-in-depth — globalThis is hardened above.
// "eval" and "Function" cannot be shadowed (strict mode / fundamental built-in).
// Direct eval(...) and Function(...) remain available — known JS-level
// limitation. Constructed code lands in the same hardened realm.
const SHADOW_PARAMS = [
    "module", "exports", "require",
    "Bun",
    "Worker", "ShadowRealm",
    "process",
];
const SHADOW_PARAMS_STR = SHADOW_PARAMS.join(",");

// ── Prototype snapshot/restore (cross-plugin pollution guard) ──────────
// Snapshot critical prototype descriptors at startup.
// After each evalPlugin: restore modified descriptors to their original state.
var _protoTracked = [
    [Object.prototype,   ["hasOwnProperty","toString","valueOf","constructor","isPrototypeOf","propertyIsEnumerable"]],
    [Array.prototype,    ["push","pop","shift","unshift","splice","slice","join","forEach","map","filter","reduce","find","findIndex","indexOf","includes","sort","flat","flatMap","concat","keys","values","entries"]],
    [Function.prototype, ["call","apply","bind","toString"]],
    [String.prototype,   ["split","replace","indexOf","includes","startsWith","endsWith","trim","slice","substring","match","search"]],
    [Promise.prototype,  ["then","catch","finally"]],
    [Error.prototype,    ["toString","message","name"]],
];
var _protoSnapshot = (function() {
    var snap = [];
    for (var i = 0; i < _protoTracked.length; i++) {
        var proto = _protoTracked[i][0], names = _protoTracked[i][1];
        for (var j = 0; j < names.length; j++) {
            var desc = _ObjectGetOwnPropertyDescriptor(proto, names[j]);
            if (desc) _ArrayPrototypePush.call(snap, [proto, names[j], desc]);
        }
    }
    return snap;
})();
function restorePrototypes() {
    // Restore modified/deleted descriptors to their startup state.
    // Added properties are NOT deleted — plugins may install polyfills at
    // registration time that their exported functions need during later calls.
    // The snapshot covers all security-critical built-in methods; a plugin
    // cannot shadow e.g. Array.prototype.push via an addition — it would
    // need to modify the existing descriptor, which IS caught here.
    for (var i = 0; i < _protoSnapshot.length; i++) {
        try { _ObjectDefineProperty(_protoSnapshot[i][0], _protoSnapshot[i][1], _protoSnapshot[i][2]); } catch(_) {}
    }
}

function evalPlugin(code, proxiedRequire) {
    var m = { exports: {} };
    // eslint-disable-next-line no-new-func -- intentional: plugin code loading
    var fn = new _RealFunction(SHADOW_PARAMS_STR, code); // NOSONAR
    try {
        fn(
            m, m.exports, proxiedRequire,
            undefined,
            undefined, undefined,
            mockedProcess
        );
    } finally {
        restorePrototypes();
    }
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

    _ObjectDefineProperty(facade, 'promises', {
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
// Use the bootstrap's readline instance (stdin bootstrap → eval → IPC on same rl)
// Falls back to creating own instance for direct `bun run native-host.cjs` usage (dev).
const rl = globalThis.__rl || readline.createInterface({ input: hostStdin });
rl.on("close", () => hostExit(0));

rl.on("line", async (line) => {
    var cmd;
    try { cmd = _JSONParse(line); } catch { return; }

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
            respond(id, { ok: true, exports: _ObjectKeys(m.exports), hash: hashCode(code) });

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
    hostStdout.write(_JSONStringify({ id, ...data }) + "\n");
}

function respondError(id, error) {
    hostStdout.write(_JSONStringify({ id, error }) + "\n");
}
