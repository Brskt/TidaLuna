// Native plugin host - sandboxed with module trust + global hardening.
//
// Plugins are loaded in a wrapper that shadows dangerous globals.
// Only whitelisted modules pass through require() directly.
// Forbidden modules are permanently blocked (no trust possible).
// Blocked modules throw TRUST_REQUIRED:<module> - Rust parses this
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
// Function constructor remains available - constructed code lands in
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
]);

// ── Permanently blocked modules (no trust possible) ─────────────────────
// No trust dialog can unlock these. require() returns an inert stub
// (FORBIDDEN_STUBS): dead-code requires can't kill a plugin; calls throw.
const FORBIDDEN_MODULES = new Set([
    "child_process", "cluster", "vm", "v8",
    "inspector", "inspector/promises", "module", "repl", "process",
    "wasi", "tty", "trace_events",
    "bun:ffi", "bun:jsc", "bun:sqlite",
]);

// ── Blocked modules (require explicit user trust) ───────────────────────
// These provide filesystem/subprocess/system/network access - require explicit user trust.
const BLOCKED_MODULES = new Set([
    "fs", "fs/promises", "os", "dgram",
    "diagnostics_channel", "worker_threads",
    // Network - raw TCP/TLS/HTTP connections to arbitrary servers
    "net", "http", "https", "http2", "tls", "dns", "dns/promises",
]);

// ── Pre-load safe + blocked modules: Bun internal closures capture refs ──
// Must happen before any globalThis mutation. Lazy init (bindings, stream
// internals) completes now with mutable prototypes; we freeze later.
// Blocked modules are pre-loaded too for their exports to be frozen (prevents
// cross-plugin mutation when two plugins both receive network trust).
SAFE_MODULES.forEach(function(id) {
    try { require(id); } catch (_) {}
});
BLOCKED_MODULES.forEach(function(id) {
    try { require(id); } catch (_) {}
});
// Pre-load worker_threads for the safe shim below to capture its exports before hardening
try { require("worker_threads"); } catch (_) {}

// Captured methods, not id.startsWith/id.slice: a plugin that replaced either on
// String.prototype could make canonicalize return "buffer", turning isSafe(anything)
// true and handing back the real, unsandboxed module. Runs at decision time: the live
// prototype is whatever the last plugin left it.
function canonicalize(id) {
    return _StringPrototypeStartsWith(id, "node:")
        ? _StringPrototypeSlice(id, 5) : id;
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

// ── Object statics, captured before anything can replace them ──────────
// Ahead of the other host-private refs further down: the env builder below runs at
// register time too, and `Object` is deliberately left unfrozen (freezePrimordials),
// while restorePrototypes only ever restores PROTOTYPE descriptors. A plugin that
// swaps Object.create would otherwise be handed the next plugin's env object.
var _ObjectFreeze = Object.freeze;
var _ObjectDefineProperty = Object.defineProperty;
var _ObjectCreate = Object.create;
var _ObjectKeys = Object.keys;
var _ObjectPrototypeHasOwnProperty = Object.prototype.hasOwnProperty;

// ── Mocked process (filtered env, no exit/kill/binding) ─────────────────
const ALLOWED_ENV_KEYS = new Set([
    "TMPDIR", "TMP", "TEMP",
    "HOME", "USERPROFILE", "PATH", "APPDATA",
]);

// A snapshot, not a live view: process.env IS globalThis.Bun.env, the same object that
// hardenBun cannot neuter, so a plugin can plant a key there and every plugin
// registered afterwards would inherit it. Read here, before any plugin has run.
const ALLOWED_ENV_SNAPSHOT = (function() {
    var pairs = [];
    Object.entries(process.env).forEach(function(pair) {
        if (ALLOWED_ENV_KEYS.has(pair[0])) pairs.push(pair);
    });
    return Object.freeze(pairs);
})();

// Extra entries arrive per plugin from the register command, never from the child's
// own env, for the same reason the snapshot exists.
//
// Null prototype on purpose. A key withheld here has to read as undefined, and
// restorePrototypes preserves added properties by design, so an inherited entry
// would let one plugin plant a value that every other plugin reads as its own.
function filterEnv(extraEnv) {
    var env = _ObjectCreate(null);
    for (var i = 0; i < ALLOWED_ENV_SNAPSHOT.length; i++) {
        env[ALLOWED_ENV_SNAPSHOT[i][0]] = ALLOWED_ENV_SNAPSHOT[i][1];
    }
    if (extraEnv) {
        var extra = _ObjectKeys(extraEnv);
        for (var j = 0; j < extra.length; j++) env[extra[j]] = extraEnv[extra[j]];
    }
    // Node's process.env answers hasOwnProperty and bundled libraries call it. Own,
    // non-enumerable and frozen: Object.keys stays clean and no plugin can reach it.
    _ObjectDefineProperty(env, "hasOwnProperty", {
        value: _ObjectPrototypeHasOwnProperty, enumerable: false,
    });
    return _ObjectFreeze(env);
}

// Libraries read process.stderr.fd/.isTTY at init (debug, chalk) and crash on
// undefined. Both writes go to stderr; stdout gets no fd because stdout carries
// the host IPC lines - fd 1 would let a plugin forge replies.
const mockedStdio = (function() {
    var realStderr = process.stderr; // captured before hardenProcess blocks it
    var toStderr = function(chunk) {
        try { realStderr.write(chunk); } catch (_) {}
        return true;
    };
    return {
        stderr: Object.freeze({ fd: 2, isTTY: false, write: toStderr }),
        stdout: Object.freeze({ isTTY: false, write: toStderr }),
    };
})();

// Read at load time so the real process answers, not the Proxy that replaces it.
// Only env differs between plugins; the rest is shared verbatim.
const mockedProcessBase = {
    platform: process.platform,
    arch: process.arch,
    version: process.version,
    versions: Object.freeze({ ...process.versions }),
    nextTick: process.nextTick.bind(process),
    hrtime: process.hrtime,
    cwd: process.cwd.bind(process),
    argv: Object.freeze([...process.argv]),
    stderr: mockedStdio.stderr,
    stdout: mockedStdio.stdout,
};

function makeMockedProcess(extraEnv) {
    return _ObjectFreeze({ env: filterEnv(extraEnv), ...mockedProcessBase });
}

const mockedProcess = makeMockedProcess(null);

// Every module that can reach a unix endpoint, not just net: http and https get there
// through request({ socketPath }). Keying discovery on net alone starves an http-only
// plugin, which can still connect but has no way left to learn where.
function reachesLocalEndpoint(trustedModules) {
    return _SetPrototypeHas(trustedModules, "net")
        || _SetPrototypeHas(trustedModules, "http")
        || _SetPrototypeHas(trustedModules, "https");
}

// The runtime dir rides in on the register command, never in the child's env, because
// globalThis.Bun.env would publish it to every plugin regardless of trust.
function endpointEnvFor(trustedModules, runtimeDir) {
    if (!runtimeDir || !reachesLocalEndpoint(trustedModules)) return mockedProcess;
    return makeMockedProcess({ XDG_RUNTIME_DIR: runtimeDir });
}

// ── Host-private refs (captured before globalThis hardening) ───────────
// After this point, host code must use these - not the globals.
var hostStdin = process.stdin;
var hostStdout = process.stdout;
var hostStderr = process.stderr;
var hostExit = process.exit.bind(process);
var hostRequire = require;
// The Object statics live further up, ahead of the env builder that needs them.
var _JSONStringify = JSON.stringify;
var _PromiseReject = Promise.reject.bind(Promise);
// Values, not the object: realFs.constants is handed to plugins on the fs facade and
// is not frozen, so reading S_IFSOCK at decision time would read a plugin's number.
var _S_IFMT = realFs.constants.S_IFMT;
var _S_IFSOCK = realFs.constants.S_IFSOCK;
// The access() mode bits fall under that same rule: they pick which gate a probe goes
// through, so a live read lets a plugin route a write-mode probe into the read gate.
var _F_OK = realFs.constants.F_OK;
var _W_OK = realFs.constants.W_OK;
var _X_OK = realFs.constants.X_OK;
// Handed out in place of the live object. Callers legitimately read these, and a shallow
// freeze covers all of them, every own value being a number.
var _fsConstantsFrozen = (function() {
    var out = {};
    var keys = _ObjectKeys(realFs.constants);
    for (var i = 0; i < keys.length; i++) out[keys[i]] = realFs.constants[keys[i]];
    return _ObjectFreeze(out);
})();
// os.tmpdir() re-reads the env on every call, and the env is plugin-writable; captured
// here, a later registration cannot take a directory of the plugin's choosing.
var hostTmpDir = require("os").tmpdir();
var _JSONParse = JSON.parse;
var _RealRequest = typeof Request !== "undefined" ? Request : undefined;
var _RealURL = typeof URL !== "undefined" ? URL : undefined;
var _RealResponse = typeof Response !== "undefined" ? Response : undefined;
var _RealTypeError = TypeError;
var _Buffer = require("buffer").Buffer;
var _setTimeout = setTimeout;
var _clearTimeout = clearTimeout;
// Prototype methods, as uncurried-this invokers bound at load. Capturing the method
// alone is not enough: `_m.call(x, ...)` reads `.call`, an own-settable property on the
// extensible method object (and, failing that, the poisonable Function.prototype.call),
// either of which a plugin can replace before it triggers host code. A bound invoker
// captures the intrinsic [[Call]] now and never reads a property at decision time, so
// `_someMethod(thisArg, ...args)` is immune. Built from bind+call captured this instant.
var _uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
var _ArrayPrototypeJoin = _uncurryThis(Array.prototype.join);
var _ArrayPrototypeForEach = _uncurryThis(Array.prototype.forEach);
var _ArrayPrototypePush = _uncurryThis(Array.prototype.push);
var _ArrayPrototypeSome = _uncurryThis(Array.prototype.some);
var _ArrayFrom = Array.from;
var _StringPrototypeSlice = _uncurryThis(String.prototype.slice);
var _FunctionPrototypeBind = _uncurryThis(Function.prototype.bind);
var _RealFunction = Function;
var _ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
var _SetPrototypeHas = _uncurryThis(Set.prototype.has);
var _MapPrototypeGet = _uncurryThis(Map.prototype.get);
var _ArrayIsArray = Array.isArray;
// Web types used to faithfully encode request bodies / rebuild responses.
var _RealHeaders = typeof Headers !== "undefined" ? Headers : undefined;
var _RealFormData = typeof FormData !== "undefined" ? FormData : undefined;
var _RealBlob = typeof Blob !== "undefined" ? Blob : undefined;
var _RealURLSearchParams = typeof URLSearchParams !== "undefined" ? URLSearchParams : undefined;
var _RealArrayBuffer = ArrayBuffer;
var _RealArrayBufferIsView = ArrayBuffer.isView;
var _RealUint8Array = Uint8Array;
var _RealDOMException = typeof DOMException !== "undefined" ? DOMException : undefined;
var _StringPrototypeStartsWith = _uncurryThis(String.prototype.startsWith);
var _StringPrototypeToLowerCase = _uncurryThis(String.prototype.toLowerCase);
var _ArrayPrototypeIndexOf = _uncurryThis(Array.prototype.indexOf);
// Real network fetch, captured before hardening neuters globalThis.fetch. Used
// ONLY for local data:/blob: URLs - network egress always takes the IPC path below.
var _RealFetch = typeof fetch === "function" ? fetch : undefined;

// ── worker_threads shim (fail-closed allowlist, no Worker) ────────────
// Exposes only communication/utility APIs. Worker constructor is blocked
// to prevent spawning unsandboxed threads with full require access.
var shimmedWorkerThreads = (function() {
    var real;
    try { real = require("worker_threads"); } catch (_) { return null; }
    var allowed = [
        "MessageChannel", "MessagePort", "BroadcastChannel",
        "receiveMessageOnPort", "markAsUncloneable", "markAsUntransferable",
        "isMainThread", "parentPort", "workerData", "threadId", "resourceLimits",
    ];
    var shim = _ObjectCreate(null);
    _ArrayPrototypeForEach(allowed, function(key) {
        if (key in real) shim[key] = real[key];
    });
    return _ObjectFreeze(shim);
})();

// ── Inert stubs for forbidden modules ─────────────────────────────────
// Bundled deps require these as dead code they never run: socket.io-client
// reaches child_process via xmlhttprequest-ssl, tty via debug, and throwing at
// require killed plugins over unused code. Members throw on CALL, not on
// property access: real code does require('child_process').spawn at top level.
// Every entry here is frozen and stateless; one shared instance is safe for
// all plugins. A stub needing mutable state belongs in makeRequireProxy instead.

// One line per member for the whole process: Rust forwards every stderr line into
// console.log uncapped and rotates only at startup; a plugin retrying on a timer
// would grow that file without bound. No plugin name in the key. The stub is
// shared, and warnInertStub's require-time line already gives attribution.
var _warnedStubCalls = new Set();

function makeForbiddenStub(id, members, answers) {
    var stub = _ObjectCreate(null);
    _ArrayPrototypeForEach(members, function(name) {
        stub[name] = function() {
            var msg = "[sandbox] " + id + "." + name + " is not available";
            // Logged as well as thrown: a plugin catching it still leaves a trace.
            // The throw is what stops the caller; logging once is enough.
            var callKey = id + "." + name;
            if (!_SetPrototypeHas(_warnedStubCalls, callKey)) {
                _warnedStubCalls.add(callKey);
                try { hostStderr.write(msg + "\n"); } catch (_) {}
            }
            throw new Error(msg);
        };
    });
    if (answers) {
        _ArrayPrototypeForEach(_ObjectKeys(answers), function(key) {
            stub[key] = answers[key];
        });
    }
    return _ObjectFreeze(stub);
}

var FORBIDDEN_STUBS = (function() {
    var t = _ObjectCreate(null);

    t["child_process"] = makeForbiddenStub("child_process", [
        "spawn", "spawnSync", "exec", "execSync",
        "execFile", "execFileSync", "fork",
    ]);

    t["tty"] = makeForbiddenStub("tty", ["ReadStream", "WriteStream"], {
        isatty: function() { return false; },
    });

    t["cluster"] = makeForbiddenStub("cluster",
        ["fork", "disconnect", "setupPrimary", "setupMaster"], {
            isPrimary: true, isMaster: true, isWorker: false,
            workers: _ObjectFreeze({}),
        });

    // url() is undefined in real Node too when no inspector is attached.
    var inspectorStub = makeForbiddenStub("inspector",
        ["Session", "open", "close", "waitForDebugger"], {
            url: function() { return undefined; },
        });
    t["inspector"] = inspectorStub;
    t["inspector/promises"] = inspectorStub;

    t["vm"] = makeForbiddenStub("vm", [
        "Script", "SourceTextModule", "SyntheticModule", "createContext",
        "isContext", "compileFunction", "runInContext", "runInNewContext",
        "runInThisContext", "measureMemory",
    ]);

    t["v8"] = makeForbiddenStub("v8", [
        "getHeapStatistics", "getHeapSpaceStatistics", "getHeapSnapshot",
        "writeHeapSnapshot", "serialize", "deserialize", "setFlagsFromString",
        "takeCoverage", "stopCoverage",
    ]);

    t["repl"] = makeForbiddenStub("repl", ["start", "REPLServer"]);
    t["wasi"] = makeForbiddenStub("wasi", ["WASI"]);
    t["trace_events"] = makeForbiddenStub("trace_events",
        ["createTracing", "getEnabledCategories"]);

    t["bun:ffi"] = makeForbiddenStub("bun:ffi", [
        "dlopen", "CString", "ptr", "toBuffer", "toArrayBuffer",
        "read", "JSCallback", "linkSymbols", "viewSource",
    ]);
    t["bun:jsc"] = makeForbiddenStub("bun:jsc", [
        "serialize", "deserialize", "describe", "describeArray", "gcAndSweep",
        "fullGC", "edenGC", "heapSize", "heapStats", "memoryUsage",
        "setTimeZone", "callerSourceOrigin",
    ]);
    t["bun:sqlite"] = makeForbiddenStub("bun:sqlite", ["Database", "deserialize"]);

    return _ObjectFreeze(t);
})();

// Per plugin, not in FORBIDDEN_STUBS: module-alias and v8-compile-cache assign
// Module.prototype._compile at load time; this must stay mutable, and a
// mutable stub can't be shared or one plugin's patches reach the next.
// Disconnected from the real Module: patches land on the throwaway prototype.
function makeModuleStub() {
    var FakeModule = function Module() {
        throw new Error("[sandbox] module.Module is not available");
    };
    FakeModule.prototype = {};
    FakeModule.builtinModules = _ObjectFreeze([]);
    FakeModule._cache = {};
    FakeModule._extensions = {};
    FakeModule.createRequire = function() {
        var msg = "[sandbox] module.createRequire is not available";
        try { hostStderr.write(msg + "\n"); } catch (_) {}
        throw new Error(msg);
    };
    FakeModule.syncBuiltinESMExports = function() {};
    return FakeModule;
}

// One line per (plugin, module): a plugin dragging a forbidden module in through
// its dependency tree stays visible at LOGS=1 without dying for it. Per-plugin sets
// rather than composite keys: forgetting a plugin is then a single delete, and no
// delimiter has to be reserved inside plugin names or module ids.
var _warnedStubs = new Map();
function warnInertStub(pluginName, canonical) {
    var seen = _MapPrototypeGet(_warnedStubs, pluginName);
    if (!seen) {
        seen = new Set();
        _warnedStubs.set(pluginName, seen);
    }
    if (_SetPrototypeHas(seen, canonical)) return;
    seen.add(canonical);
    try {
        hostStderr.write("[sandbox] " + pluginName + ": require('" + canonical
            + "') is blocked - returned an inert stub, calls into it will throw\n");
    } catch (_) {}
}

// ── Harden globalThis.process via Proxy ───────────────────────────────
// Cannot fully replace - Bun's network modules need process.nextTick etc.
// Proxy blocks dangerous properties while passing safe internals through.
// process.binding() is filtered to only the names http/net/tls need at
// require-time (http_parser, uv). Since SAFE_MODULES are pre-loaded above,
// binding is never actually called again - the filter is defense-in-depth.
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
    var origBinding = _FunctionPrototypeBind(realProcess.binding, realProcess);
    var safeBinding = function(name) {
        if (!_SetPrototypeHas(allowedBindings, name))
            throw new Error("[sandbox] process.binding('" + name + "') is not allowed");
        return origBinding(name);
    };
    var filteredEnv = mockedProcess.env; // already frozen + filtered by ALLOWED_ENV_KEYS
    var processProxy = new Proxy(realProcess, {
        get: function(target, prop) {
            if (_SetPrototypeHas(blockedKeys, prop))
                throw new Error("[sandbox] process." + prop + " is not available");
            if (prop === "env") return filteredEnv;
            if (prop === "binding") return safeBinding;
            var val = target[prop];
            return typeof val === "function" ? _FunctionPrototypeBind(val, target) : val;
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
// console.log writes to stdout - the same fd as host IPC (JSON lines).
// A plugin could forge IPC responses via console.log. Redirect all
// console output to stderr only.
;(function hardenConsole() {
    var safeConsole = _ObjectCreate(null);
    var noop = function() {};
    var toStderr = function() {
        try {
            hostStderr.write(_ArrayPrototypeJoin(arguments, " ") + "\n");
        } catch (_) {}
    };
    _ArrayPrototypeForEach(["log", "warn", "error", "info", "debug",
     "trace", "dir", "dirxml", "table", "assert", "time", "timeLog", "timeEnd",
     "count", "countReset"], function(m) { safeConsole[m] = toStderr; });
    _ArrayPrototypeForEach(["group", "groupCollapsed", "groupEnd",
     "clear", "profile", "profileEnd", "timeStamp"], function(m) { safeConsole[m] = noop; });
    _ObjectFreeze(safeConsole);
    _ObjectDefineProperty(globalThis, "console", {
        value: safeConsole, writable: false, configurable: false,
    });
})();

// ── Neuter Async/Generator constructors ────────────────────────────────
// Prevents import() bypass via (async function(){}).constructor("return await import('fs')")().
// Function.prototype.constructor is NOT touched - breaks stream/events/util.
;(function neuterAsyncConstructors() {
    var ctors = [
        (async function(){}).constructor,
        (function*(){}).constructor,
        (async function*(){}).constructor,
    ];
    _ArrayPrototypeForEach(ctors, function(ctor) {
        _ObjectDefineProperty(ctor.prototype, "constructor", {
            value: undefined, writable: false, configurable: false,
        });
    });
})();

// ── Proxied require factory ─────────────────────────────────────────────
function makeRequireProxy(trustedModules, sandboxedFs, dataDir, pluginName, pluginProcess) {
    var moduleStub = null; // lazy, per plugin - see makeModuleStub
    return function proxiedRequire(id) {
        // Virtual module: plugin data directory
        if (id === "@luna/native-data" || id === "node:@luna/native-data")
            return _ObjectFreeze({ dir: dataDir });

        // Return hardened console (stderr-only) - real require('console') is stdout-backed
        if (id === "console" || id === "node:console") return globalThis.console;

        if (isSafe(id)) return require(id);

        var canonical = canonicalize(id);

        if (isForbidden(id)) {
            // Not a capability: already the plugin's ambient `process`. The
            // globalThis Proxy would throw on .stderr where the ambient one works.
            if (canonical === "process") return pluginProcess;

            // Memoized; require('module') === require('module') within a plugin.
            if (canonical === "module") {
                warnInertStub(pluginName, canonical);
                if (!moduleStub) moduleStub = makeModuleStub();
                return moduleStub;
            }

            var stub = FORBIDDEN_STUBS[canonical];
            if (stub) {
                warnInertStub(pluginName, canonical);
                return stub;
            }
            throw new Error("[sandbox] require('" + canonical + "') is permanently blocked");
        }

        if (isBlocked(id)) {
            // worker_threads: return safe shim (no Worker constructor) when trusted
            if (canonical === "worker_threads") {
                if (!shimmedWorkerThreads)
                    throw new Error("[sandbox] worker_threads is not available in this environment");
                if (trustedModules.has(id) || trustedModules.has(canonical))
                    return shimmedWorkerThreads;
                throw new Error("TRUST_REQUIRED:" + canonical);
            }
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

        // Relative/absolute paths - blocked. Captured startsWith for the same reason
        // as canonicalize, even though the fall-through below also throws.
        if (_StringPrototypeStartsWith(id, ".") || _StringPrototypeStartsWith(id, "/")
            || /^[a-zA-Z]:/.test(id)) {
            throw new Error("[sandbox] require('" + id + "') blocked: paths not allowed");
        }

        // Unknown third-party - blocked
        throw new Error("[sandbox] require('" + id + "') blocked: not in whitelist");
    };
}

// ── import() pre-check via Bun.Transpiler ───────────────────────────────
var transpiler;
try { transpiler = new Bun.Transpiler({ loader: "js" }); } catch (e) { /* fallback below */ }

function containsDynamicImport(code) {
    if (!transpiler) return true; // can't verify - block (fail-closed)
    try {
        var result = transpiler.scan(code);
        // Captured some: this is the only gate on dynamic import(), which otherwise
        // bypasses the require proxy entirely. A plugin that replaced Array.prototype.some
        // with a false-returning stub would open that door for every later registration.
        return _ArrayPrototypeSome(result.imports,
            function(i) { return i.kind === "dynamic-import"; });
    } catch (e) {
        return true; // unparseable - block (fail-closed)
    }
}

// ── Harden eval - scan for dynamic import() before delegating ─────────
// eval("import('fs')") bypasses containsDynamicImport (AST scan of source)
// because the import() is inside a string literal. This wrapper scans the
// final runtime string. Runs on the evaluated string: concatenation
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

// ── Harden Function constructor - scan for dynamic import() ───────────
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
    // Keep writable: bundled strict-mode libs do `fn.constructor = fn` (UTIF in node-vibrant),
    // and a non-writable inherited slot throws on that. Value stays SafeFunction; it's still scanned.
    _ObjectDefineProperty(RealFunction.prototype, "constructor", {
        value: SafeFunction, writable: true, configurable: false,
    });
    _ObjectDefineProperty(globalThis, "Function", {
        value: SafeFunction, writable: false, configurable: false,
    });
})();

// ── Harden globalThis.Bun - neuter dangerous methods in-place ─────────
// Bun is non-configurable and can't be replaced; its writable methods
// (spawn/file/write/...) ARE neutered below. Bun.fetch/Bun.env are
// writable:false (can't neuter), but env is scrubbed at spawn and fetch
// egress is the IPC path's job; closing Bun.fetch direct = OS-level (backlog).
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
        "udpSocket", "which", // raw UDP egress + PATH binary enum
        "env", "embeddedFiles",
        "FFI", "secrets", // native dlopen (escape) + OS keychain
        "SQL", "RedisClient", "S3Client", "postgres", // DB/S3 network clients
        "Glob", "FileSystemRouter", "Archive", "generateHeapSnapshot", "unsafe", // fs enum / memory outside sandbox
    ];
    _ArrayPrototypeForEach(DANGEROUS, function(key) {
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

// ── Block globalThis.require/module/exports - prevent proxy bypass ────
;(function hardenGlobalRequire() {
    _ArrayPrototypeForEach(["require", "module", "exports", "__dirname", "__filename"], function(prop) {
        _ObjectDefineProperty(globalThis, prop, {
            get: function() { throw new Error("[sandbox] globalThis." + prop + " is not available"); },
            configurable: false,
        });
    });
})();

// ── Block fetch - native plugins must use require('http'/'https') with trust dialog ──
;(function hardenFetch() {
    _ObjectDefineProperty(globalThis, "fetch", {
        value: function() { throw new Error("[sandbox] fetch is not available - use require('http') or require('https')"); },
        writable: false, configurable: false,
    });
})();

// ── Block realm creators and dangerous network globals on globalThis ──
;(function hardenGlobals() {
    // Realm creators + WebSocket - no upstream plugin uses the browser WebSocket global
    // (DiscordRPC uses net IPC, ws-based plugins use require('ws') which goes through
    // http/net SAFE_MODULES, not globalThis.WebSocket)
    _ArrayPrototypeForEach(["Worker", "ShadowRealm", "WebSocket"], function(prop) {
        _ObjectDefineProperty(globalThis, prop, {
            value: undefined, writable: false, configurable: false,
        });
    });
    // Network globals with no legitimate plugin use
    _ArrayPrototypeForEach(["XMLHttpRequest", "EventSource", "BroadcastChannel"], function(prop) {
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
    _ArrayPrototypeForEach(fullFreeze, function(obj) {
        try { _ObjectFreeze(obj); } catch (_) {}
        try { if (obj.prototype) _ObjectFreeze(obj.prototype); } catch (_) {}
    });

    // Built-in prototypes (Object, Array, Function, String, Promise, Error, etc.)
    // are NOT frozen - npm packages bundled into plugins assign to inherited
    // property names (e.g. node-inspect-extracted), which throws in strict mode
    // when the prototype is frozen. The shared-module mutation vector is covered
    // by freezeSafeModuleExports() below instead.
})();

// ── Freeze module exports (safe + blocked) ────────────────────────────
// Prevents plugins from mutating shared module exports to influence
// host behavior or other plugins. Blocked modules are frozen too so
// cross-plugin mutation is prevented when multiple plugins have trust.
;(function freezeModuleExports() {
    function freezeSet(set) {
        set.forEach(function(id) {
            try {
                var mod = hostRequire(id);
                if (mod && typeof mod === "object") _ObjectFreeze(mod);
            } catch (_) {}
        });
    }
    freezeSet(SAFE_MODULES);
    freezeSet(BLOCKED_MODULES);
})();

// ── IPC fetch (sanctioned egress: child -> Rust -> network) ───────────
// Network-trusted plugins get this as their `fetch` shadow: emits net.fetch,
// resolves a real Response from Rust's reply. Does NOT close Bun.fetch (OS-level).
var pendingFetches = new Map();
var nextFetchId = 1;
var FETCH_GUARD_MS = 60000;

function normalizeHeaders(h) {
    // Ordered [name, value] pairs: preserves duplicate header names and avoids the
    // __proto__ hazard of a plain object. Headers tested first (a Map's forEach differs).
    var out = [];
    if (!h) return out;
    if (_RealHeaders && h instanceof _RealHeaders) {
        h.forEach(function(v, k) { _ArrayPrototypePush(out, [k, String(v)]); });
    } else if (_ArrayIsArray(h)) {
        for (var i = 0; i < h.length; i++) {
            if (h[i]) _ArrayPrototypePush(out, [String(h[i][0]), String(h[i][1])]);
        }
    } else if (typeof h === "object") {
        var keys = _ObjectKeys(h);
        for (var j = 0; j < keys.length; j++) {
            _ArrayPrototypePush(out, [keys[j], String(h[keys[j]])]);
        }
    }
    return out;
}

function headersHave(pairs, lowerName) {
    for (var i = 0; i < pairs.length; i++) {
        if (_StringPrototypeToLowerCase(pairs[i][0]) === lowerName) return true;
    }
    return false;
}

// Serialize a FormData to a multipart/form-data Buffer with the given boundary.
async function formDataToBuffer(fd, boundary) {
    var entries = [];
    fd.forEach(function(value, key) { _ArrayPrototypePush(entries, [key, value]); });
    var chunks = [];
    for (var i = 0; i < entries.length; i++) {
        var key = entries[i][0], value = entries[i][1];
        var head = "--" + boundary + "\r\nContent-Disposition: form-data; name=\"" + key + "\"";
        if (_RealBlob && value instanceof _RealBlob) {
            head += "; filename=\"" + (value.name || "blob") + "\"\r\nContent-Type: "
                + (value.type || "application/octet-stream") + "\r\n\r\n";
            _ArrayPrototypePush(chunks, _Buffer.from(head, "utf8"));
            var ab = await value.arrayBuffer();
            _ArrayPrototypePush(chunks, _Buffer.from(new _RealUint8Array(ab)));
            _ArrayPrototypePush(chunks, _Buffer.from("\r\n", "utf8"));
        } else {
            head += "\r\n\r\n" + String(value) + "\r\n";
            _ArrayPrototypePush(chunks, _Buffer.from(head, "utf8"));
        }
    }
    _ArrayPrototypePush(chunks, _Buffer.from("--" + boundary + "--\r\n", "utf8"));
    return _Buffer.concat(chunks);
}

// Encode a request body to base64, returning an implied content-type when the
// body type sets one (URLSearchParams, FormData, Blob).
async function encodeBody(body) {
    if (body == null) return { b64: null, contentType: null };
    if (typeof body === "string") {
        return { b64: _Buffer.from(body, "utf8").toString("base64"), contentType: null };
    }
    if (_RealArrayBufferIsView(body)) {
        // Any typed-array view or DataView: take the underlying bytes verbatim.
        return {
            b64: _Buffer.from(body.buffer, body.byteOffset, body.byteLength).toString("base64"),
            contentType: null,
        };
    }
    if (body instanceof _RealArrayBuffer) {
        return { b64: _Buffer.from(new _RealUint8Array(body)).toString("base64"), contentType: null };
    }
    if (_RealBlob && body instanceof _RealBlob) {
        var ab = await body.arrayBuffer();
        return {
            b64: _Buffer.from(new _RealUint8Array(ab)).toString("base64"),
            contentType: body.type || null,
        };
    }
    if (_RealURLSearchParams && body instanceof _RealURLSearchParams) {
        return {
            b64: _Buffer.from(body.toString(), "utf8").toString("base64"),
            contentType: "application/x-www-form-urlencoded;charset=UTF-8",
        };
    }
    if (_RealFormData && body instanceof _RealFormData) {
        var boundary = "----TidaLunarBoundary" + (nextFetchId++).toString(36);
        var buf = await formDataToBuffer(body, boundary);
        return { b64: buf.toString("base64"), contentType: "multipart/form-data; boundary=" + boundary };
    }
    // Unknown body type: coerce to string (fetch's last-resort behavior).
    return { b64: _Buffer.from(String(body), "utf8").toString("base64"), contentType: null };
}

function makeAbortError() {
    if (_RealDOMException) return new _RealDOMException("The operation was aborted.", "AbortError");
    var err = new _RealTypeError("The operation was aborted.");
    err.name = "AbortError";
    return err;
}

// Remove a pending fetch and tear down its timer + abort listener in one place,
// making every settle path symmetric: timeout, transport-fail, abort, result.
function settle(reqId) {
    var entry = pendingFetches.get(reqId);
    if (!entry) return null;
    pendingFetches.delete(reqId);
    _clearTimeout(entry.timer);
    if (entry.signal && entry.onAbort) {
        try { entry.signal.removeEventListener("abort", entry.onAbort); } catch (_) {}
    }
    return entry;
}

// Tell Rust to drop an in-flight net.fetch (the child gave up: abort or timeout);
// it stops doing network work nobody awaits.
function sendCancel(reqId) {
    try {
        hostStdout.write(_JSONStringify({ type: "net.fetch.cancel", reqId: reqId }) + "\n");
    } catch (_) {}
}

function makeIpcFetch(pluginName) {
    return async function ipcFetch(input, init) {
        var url, method = "GET", headers = [], body, redirect, signal;
        if (_RealRequest && input instanceof _RealRequest) {
            url = input.url;
            method = input.method;
            headers = normalizeHeaders(input.headers);
            redirect = input.redirect;
            signal = input.signal;
            // The Request carries the body unless init overrides it; clone to keep
            // the caller's Request from being consumed.
            if (!(init && init.body != null) && input.body != null) {
                try { body = await input.clone().arrayBuffer(); } catch (_) {}
            }
        } else if (input && typeof input === "object" && typeof input.url === "string") {
            url = input.url;
            if (input.method) method = input.method;
            if (input.headers) headers = normalizeHeaders(input.headers);
        } else {
            url = String(input);
        }
        if (init) {
            if (init.method) method = init.method;
            if (init.headers) headers = normalizeHeaders(init.headers);
            if (init.body != null) body = init.body;
            if (init.redirect) redirect = init.redirect;
            if (init.signal !== undefined) signal = init.signal;
        }

        // Local schemes resolve in-process (no egress, no IPC, no trust concern).
        if (_RealFetch
            && (_StringPrototypeStartsWith(url, "data:")
                || _StringPrototypeStartsWith(url, "blob:"))) {
            return _RealFetch(input, init);
        }

        if (signal && signal.aborted) throw makeAbortError();

        var enc = await encodeBody(body);
        if (enc.contentType && !headersHave(headers, "content-type")) {
            _ArrayPrototypePush(headers, ["content-type", enc.contentType]);
        }

        return await new Promise(function(resolve, reject) {
            var reqId = nextFetchId++;
            var timer = _setTimeout(function() {
                var e = settle(reqId);
                if (e) {
                    sendCancel(reqId);
                    e.reject(new _RealTypeError("[sandbox] fetch timed out"));
                }
            }, FETCH_GUARD_MS);
            var onAbort = null;
            if (signal) {
                onAbort = function() {
                    var e = settle(reqId);
                    if (!e) return;
                    sendCancel(reqId);
                    e.reject(makeAbortError());
                };
                try { signal.addEventListener("abort", onAbort); } catch (_) {}
            }
            pendingFetches.set(reqId, {
                resolve: resolve, reject: reject, timer: timer, signal: signal, onAbort: onAbort,
            });
            try {
                hostStdout.write(_JSONStringify({
                    type: "net.fetch", reqId: reqId, plugin: pluginName,
                    url: url, method: method, headers: headers, body: enc.b64,
                    redirect: redirect || "follow",
                }) + "\n");
            } catch (e) {
                var en = settle(reqId);
                if (en) en.reject(new _RealTypeError("[sandbox] fetch transport failed"));
            }
        });
    };
}

function handleFetchResult(cmd) {
    var entry = settle(cmd.reqId);
    if (!entry) return;
    if (!cmd.ok) {
        entry.reject(new _RealTypeError(cmd.error || "fetch failed"));
        return;
    }
    try {
        var hasBody = typeof cmd.body === "string" && cmd.body.length > 0;
        var bodyArg = hasBody ? _Buffer.from(cmd.body, "base64") : null;
        // Rebuild Headers via append: duplicate names (Set-Cookie/Link) survive.
        var headers = _RealHeaders ? new _RealHeaders() : {};
        if (_ArrayIsArray(cmd.headers)) {
            for (var i = 0; i < cmd.headers.length; i++) {
                var pair = cmd.headers[i];
                if (!pair) continue;
                if (_RealHeaders) headers.append(pair[0], pair[1]);
                else headers[pair[0]] = pair[1];
            }
        }
        // The Response ctor rejects a status outside 200-599; build with a safe
        // status, then restore the real one (1xx/999/...) on the instance for the
        // plugin to see the true status without a thrown error.
        var realStatus = typeof cmd.status === "number" ? cmd.status : 200;
        var ctorStatus = realStatus >= 200 && realStatus <= 599 ? realStatus : 200;
        var res = new _RealResponse(bodyArg, {
            status: ctorStatus,
            statusText: cmd.statusText || "",
            headers: headers,
        });
        try {
            if (ctorStatus !== realStatus) {
                _ObjectDefineProperty(res, "status", { value: realStatus, configurable: true });
                _ObjectDefineProperty(res, "ok", {
                    value: realStatus >= 200 && realStatus < 300, configurable: true,
                });
            }
            _ObjectDefineProperty(res, "url", { value: cmd.url || "", configurable: true });
            _ObjectDefineProperty(res, "redirected", { value: !!cmd.redirected, configurable: true });
        } catch (_) {}
        entry.resolve(res);
    } catch (e) {
        entry.reject(new _RealTypeError("[sandbox] bad fetch response: " + (e && e.message)));
    }
}

// ── Eval wrapper ────────────────────────────────────────────────────────
// Shadows dangerous globals as parameters set to undefined.
// Parameter shadows are defense-in-depth - globalThis is hardened above.
// "eval" and "Function" cannot be shadowed (strict mode / fundamental built-in).
// Direct eval(...) and Function(...) remain available - known JS-level
// limitation. Constructed code lands in the same hardened realm.
const SHADOW_PARAMS = [
    "module", "exports", "require",
    "Bun",
    "Worker", "ShadowRealm",
    "process",
    "fetch",
];
const SHADOW_PARAMS_STR = SHADOW_PARAMS.join(",");

// ── Prototype snapshot/restore (cross-plugin pollution guard) ──────────
// Snapshot critical prototype descriptors at startup.
// After each evalPlugin: restore modified descriptors to their original state.
var _protoTracked = [
    [Object.prototype,   ["hasOwnProperty","toString","valueOf","constructor","isPrototypeOf","propertyIsEnumerable"]],
    [Array.prototype,    ["push","pop","shift","unshift","splice","slice","join","forEach","map","filter","reduce","find","findIndex","indexOf","includes","sort","flat","flatMap","concat","keys","values","entries"]],
    [Function.prototype, ["call","apply","bind","toString","constructor"]],
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
            if (desc) _ArrayPrototypePush(snap, [proto, names[j], desc]);
        }
    }
    return snap;
})();
function restorePrototypes() {
    // Restore modified descriptors to startup state. Added props survive
    // (plugins' register-time polyfills need them later); safe because a
    // plugin can't shadow a security-critical method by addition - only by
    // modifying the existing descriptor, which IS caught here.
    for (var i = 0; i < _protoSnapshot.length; i++) {
        try { _ObjectDefineProperty(_protoSnapshot[i][0], _protoSnapshot[i][1], _protoSnapshot[i][2]); } catch(_) {}
    }
}

function evalPlugin(code, proxiedRequire, pluginFetch, pluginProcess) {
    var m = { exports: {} };
    // eslint-disable-next-line no-new-func -- intentional: plugin code loading
    var fn = new _RealFunction(SHADOW_PARAMS_STR, code); // NOSONAR
    try {
        fn(
            m, m.exports, proxiedRequire,
            undefined,
            undefined, undefined,
            pluginProcess,
            pluginFetch
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
// Blocks symlink/link/readlink, delete hors dataDir, options.fd/fs. realpath answers
// inside the sandbox, and for the IPC dirs of a plugin trusted to open an endpoint.

// Lexical only: URL/type/UNC handling and `..` collapse, without following symlinks.
// `..` is still resolved away, so containment cannot be escaped, but a symlink keeps
// its own path - which is what lets a bridged endpoint under a disclosed dir be seen.
function lexicalFsPath(p) {
    if (p instanceof URL
        || (typeof p === 'string' && _StringPrototypeStartsWith(p, 'file:')))
        p = fileURLToPath(p);
    if (typeof p !== 'string')
        throw new Error("[sandbox] invalid path type");
    var resolved = pathMod.resolve(p);
    // Block UNC/device paths on Windows - they bypass startsWith containment checks
    if (process.platform === 'win32'
        && _StringPrototypeStartsWith(pathMod.win32.parse(resolved).root, '\\\\'))
        throw new Error("[sandbox] UNC/device paths are not allowed");
    return resolved;
}

function realpathOrAncestor(resolved) {
    try {
        return realFs.realpathSync(resolved);
    } catch (_) {
        return resolveFromExistingAncestor(resolved);
    }
}

function canonicalizeFsPath(p) {
    return realpathOrAncestor(lexicalFsPath(p));
}

// Resolves the parent and leaves the final component as written, which is the identity a
// removal acts on: `unlink` takes a directory entry away, not the file it names. Resolving
// the leaf would judge a symlink's target and then remove the link, and denying that case
// outright would refuse a plugin its own links inside its own dataDir.
function canonicalizeLeafPath(p) {
    var lexical = lexicalFsPath(p);
    var parent = pathMod.dirname(lexical);
    // A filesystem root names no entry; leave it whole for the gate to refuse on its own.
    if (parent === lexical) return lexical;
    return pathMod.join(realpathOrAncestor(parent), pathMod.basename(lexical));
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

// Captured String.prototype.startsWith, not the live one: plugin code runs again on
// every `call`, and restorePrototypes only fires at the end of a register.
function isInDirs(real, dirs) {
    for (var i = 0; i < dirs.length; i++) {
        if (real === dirs[i]
            || _StringPrototypeStartsWith(real, dirs[i] + pathMod.sep)) return true;
    }
    return false;
}

// Separate from assertRead because the probes below need the verdict, not a throw.
function isReadable(real, dataDirs, grants) {
    if (isInDirs(real, dataDirs)) return true;
    if (grants.readFiles.has(real)) return true;
    if (grants.writeFiles.has(real)) return true;
    return isInDirs(real, _ArrayFrom(grants.dirs));
}

// Every gate below returns the path it authorized, and its caller operates on that one.
// Handing back nothing left each facade method to re-derive a target from the argument,
// which is a second path: canonicalization is not the identity, so the two disagree
// wherever a symlink or a forged resolution stands between them, and the check then
// answers about one file while the operation touches another.
function assertRead(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (!isReadable(real, dataDirs, grants))
        throw new Error("[sandbox] fs read denied: " + p);
    return real;
}

function assertWrite(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return real;
    if (grants.writeFiles.has(real)) return real;
    if (isInDirs(real, _ArrayFrom(grants.dirs))) return real;
    throw new Error("[sandbox] fs write denied: " + p);
}

function assertDelete(p, dataDirs) {
    var real = canonicalizeLeafPath(p);
    if (isInDirs(real, dataDirs)) return real;
    throw new Error("[sandbox] fs delete denied: " + p);
}

function assertMkdir(p, dataDirs, grants) {
    var real = canonicalizeFsPath(p);
    if (isInDirs(real, dataDirs)) return real;
    if (isInDirs(real, _ArrayFrom(grants.dirs))) return real;
    throw new Error("[sandbox] fs mkdir denied: " + p);
}

function rejectUnsafeOpts(opts) {
    if (opts && typeof opts === 'object') {
        if ('fd' in opts) throw new Error("[sandbox] options.fd not allowed");
        if ('fs' in opts) throw new Error("[sandbox] options.fs not allowed");
    }
}

// Where a local IPC endpoint can live: the session runtime dir the register command
// carried, plus the temp dir the child was started in and reports through cwd(). Built
// per plugin, and an empty list is how a plugin without endpoint trust is expressed -
// there is no separate flag to disagree with it.
function probeDirsFor(runtimeDir) {
    var out = [];
    function add(dir) {
        if (dir && _ArrayPrototypeIndexOf(out, dir) === -1) _ArrayPrototypePush(out, dir);
    }
    var candidates = [runtimeDir, hostTmpDir];
    for (var i = 0; i < candidates.length; i++) {
        if (!candidates[i] || typeof candidates[i] !== "string") continue;
        var lexical = pathMod.resolve(candidates[i]);
        var real;
        try { real = realFs.realpathSync(lexical); } catch (_) { continue; }
        // Both names for the one directory. existsSync compares the socket's lexical
        // path, and a caller may hand us either the realpath or a path with a symlinked
        // prefix (macOS TMPDIR under /var, its realpath under /private/var). Only dirs
        // that resolve are added, so this is the same disclosed dir by two spellings.
        add(real);
        add(lexical);
    }
    return out;
}

// One gate for every realpath the facades expose (sync, its .native, the promises twin),
// keeping the two facades from drifting into separate policies. Admits the disclosed dir
// itself and never its children: an endpoint path is joined by the caller, not resolved.
function makeGatedRealpath(dataDirs, grants, ipcDirs, resolve) {
    return function(p, o) {
        var real = canonicalizeFsPath(p);
        if (!isReadable(real, dataDirs, grants)
            && _ArrayPrototypeIndexOf(ipcDirs, real) === -1)
            throw new Error("[sandbox] fs realpath denied: " + p);
        return resolve(real, o);
    };
}

// Reads the mode rather than calling stats.isSocket(): that method lives on a prototype
// shared by every stat in the process, which a plugin reaches through the facade's own
// statSync and can replace. `mode` is an own data property of each Stats object.
//
// Follows symlinks deliberately: a bridged endpoint (SSH, Flatpak) is a link to the
// socket, so lstat would answer about the link instead.
function isSocket(real) {
    if (_S_IFSOCK === undefined) return false; // no such file type on this platform
    try { return (realFs.statSync(real).mode & _S_IFMT) === _S_IFSOCK; }
    catch (_) { return false; }
}

// One line per cause for the whole process, like _warnedStubCalls: a plugin probing on
// a timer would otherwise grow the log without bound.
var _warnedProbeRejects = new Set();
function warnProbeReject(p, cause) {
    if (_SetPrototypeHas(_warnedProbeRejects, cause)) return;
    _warnedProbeRejects.add(cause);
    try {
        hostStderr.write("[sandbox] fs existsSync answered false for " + p
            + " because the path was rejected: " + cause + "\n");
    } catch (_) {}
}

function makeSandboxedFs(dataDirs, grants, ipcDirs) {
    function checkRead(p) { return assertRead(p, dataDirs, grants); }
    function checkWrite(p) { return assertWrite(p, dataDirs, grants); }
    function checkDelete(p) { return assertDelete(p, dataDirs); }
    function checkMkdir(p) { return assertMkdir(p, dataDirs, grants); }

    function gatedRealpath(resolve) {
        return makeGatedRealpath(dataDirs, grants, ipcDirs, resolve);
    }

    var facade = {
        readFileSync: function(p, o) { return realFs.readFileSync(checkRead(p), o); },
        writeFileSync: function(p, d, o) { return realFs.writeFileSync(checkWrite(p), d, o); },
        existsSync: function(p) {
            // Real existsSync answers false on every error, EACCES included, so a
            // denial is an answer here rather than a throw. A rejected path shape is
            // announced once: unannounced, a Windows named pipe - refused by the UNC
            // guard - would be indistinguishable from an endpoint that is simply gone.
            var lexical;
            try {
                lexical = lexicalFsPath(p);
            } catch (e) {
                warnProbeReject(p, e.message);
                return false;
            }
            var real = realpathOrAncestor(lexical);
            if (isReadable(real, dataDirs, grants)) return realFs.existsSync(real);
            // Outside the sandbox, an endpoint inside a disclosed dir and nothing else.
            // Containment is checked on the lexical path so a bridged endpoint (a symlink
            // under the disclosed dir pointing at a socket elsewhere) is admitted, while
            // the socket type is tested on the resolved target - only sockets ever pass,
            // and a plugin has no write access to a disclosed dir to plant such a link.
            // Whoever holds the trust that produced those dirs could learn the same by
            // connecting; whoever does not cannot use the answer.
            return isInDirs(lexical, ipcDirs) && isSocket(real);
        },
        realpathSync: gatedRealpath(realFs.realpathSync),
        mkdirSync: function(p, o) { return realFs.mkdirSync(checkMkdir(p), o); },
        unlinkSync: function(p) { return realFs.unlinkSync(checkDelete(p)); },
        rmSync: function(p, o) { return realFs.rmSync(checkDelete(p), o); },
        statSync: function(p, o) { return realFs.statSync(checkRead(p), o); },
        accessSync: function(p, mode) {
            var m = (mode === undefined) ? _F_OK : mode;
            if (m & _X_OK)
                throw new Error("[sandbox] fs X_OK denied");
            return realFs.accessSync((m & _W_OK) ? checkWrite(p) : checkRead(p), mode);
        },
        createWriteStream: function(p, o) {
            rejectUnsafeOpts(o);
            return realFs.createWriteStream(checkWrite(p), o);
        },
        constants: _fsConstantsFrozen,
    };

    // Callers feature-detect .native before using it (typescript, resolve); a facade
    // without one fails their check and takes a path they only meant as a fallback.
    facade.realpathSync.native =
        gatedRealpath(realFs.realpathSync.native || realFs.realpathSync);

    _ObjectDefineProperty(facade, 'promises', {
        get: function() { return makeSandboxedFsPromises(dataDirs, grants, ipcDirs); },
        enumerable: true,
    });

    return facade;
}

function makeSandboxedFsPromises(dataDirs, grants, ipcDirs) {
    function checkRead(p) { return assertRead(p, dataDirs, grants); }
    function checkWrite(p) { return assertWrite(p, dataDirs, grants); }
    function checkDelete(p) { return assertDelete(p, dataDirs); }
    function checkMkdir(p) { return assertMkdir(p, dataDirs, grants); }

    var gatedRealpath = makeGatedRealpath(dataDirs, grants, ipcDirs, realFs.promises.realpath);

    return {
        // Rejects rather than throwing: this twin is awaited, unlike the sync one. The
        // captured reject, so a swapped Promise.reject cannot turn a denial into a
        // resolved promise.
        realpath: function(p, o) {
            try { return gatedRealpath(p, o); } catch (e) { return _PromiseReject(e); }
        },
        readFile: function(p, o) { return realFs.promises.readFile(checkRead(p), o); },
        writeFile: function(p, d, o) { return realFs.promises.writeFile(checkWrite(p), d, o); },
        mkdir: function(p, o) { return realFs.promises.mkdir(checkMkdir(p), o); },
        stat: function(p, o) { return realFs.promises.stat(checkRead(p), o); },
        unlink: function(p) { return realFs.promises.unlink(checkDelete(p)); },
        rm: function(p, o) { return realFs.promises.rm(checkDelete(p), o); },
        access: function(p, mode) {
            var m = (mode === undefined) ? _F_OK : mode;
            if (m & _X_OK)
                return Promise.reject(new Error("[sandbox] fs X_OK denied"));
            return realFs.promises.access((m & _W_OK) ? checkWrite(p) : checkRead(p), mode);
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
// Reuse bootstrap readline if available, fall back to own instance for standalone dev usage.
const rl = globalThis.__rl || readline.createInterface({ input: hostStdin });
rl.on("close", () => hostExit(0));

rl.on("line", async (line) => {
    var cmd;
    try { cmd = _JSONParse(line); } catch { return; }

    var id = cmd.id;
    var type = cmd.type;
    if (type === "net.fetch.result") { handleFetchResult(cmd); return; }
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

            // Own keys only: for...in walks Object.prototype, and restorePrototypes
            // keeps added properties, so a plugin planting Object.prototype.fs = true
            // would grant that module to every plugin registered afterwards.
            var trustedModules = new Set();
            if (trust) {
                var trustKeys = _ObjectKeys(trust);
                for (var ti = 0; ti < trustKeys.length; ti++) {
                    if (trust[trustKeys[ti]] === true) trustedModules.add(trustKeys[ti]);
                }
            }

            // Discovery follows the capability: a plugin that can already open a local
            // endpoint may learn where one is. Empty list for everyone else.
            var ipcDirs = reachesLocalEndpoint(trustedModules)
                ? probeDirsFor(cmd.runtimeDir)
                : [];

            // Build sandboxed fs restricted to plugin dataDir + grants
            var sandboxedFs = null;
            if (dataDir) {
                realFs.mkdirSync(dataDir, { recursive: true });
                var canonicalDataDir = realFs.realpathSync(dataDir);
                var grants = getGrantStore(name);
                sandboxedFs = makeSandboxedFs([canonicalDataDir], grants, ipcDirs);
            }

            // A register rebuilds this plugin's JS state on the same long-lived Bun
            // process (disable/re-enable, reinstall, code update all land here), so
            // forget its warned modules too or the inert-stub lines never reappear.
            _warnedStubs.delete(name);

            // require('process') and the shadow parameter share this object. The
            // globalThis Proxy cannot: it is one object for every plugin at once and
            // keeps the narrow env. A dependency that reads env off the global therefore
            // sees no runtime dir even here - fails closed, rather than handing it to
            // the plugins that were refused it.
            var pluginProcess = endpointEnvFor(trustedModules, cmd.runtimeDir);

            var proxiedRequire = makeRequireProxy(trustedModules, sandboxedFs, dataDir, name, pluginProcess);
            // fetch gated on network trust (require('http')/'https' group), baked per-plugin
            // into the eval env - no shared mutable state to race across concurrent calls.
            var hasNetworkTrust = _SetPrototypeHas(trustedModules, "http")
                || _SetPrototypeHas(trustedModules, "https");
            var pluginFetch = hasNetworkTrust
                ? makeIpcFetch(name)
                : function() { throw new Error("[sandbox] fetch requires network trust - grant http/https"); };
            var m = evalPlugin(code, proxiedRequire, pluginFetch, pluginProcess);
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
            _warnedStubs.delete(cmd.name);
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
