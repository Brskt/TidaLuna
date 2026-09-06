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
var _ObjectGetPrototypeOf = Object.getPrototypeOf;
var _ObjectSetPrototypeOf = Object.setPrototypeOf;
var _ObjectPrototypeRef = Object.prototype;

// Working arrays the host writes into, built with no prototype so a plugin-planted
// Array.prototype["0"] accessor has nothing to intercept. The order is the whole point:
// nulling after the writes is too late, the first one already reached the setter. Note
// this costs the array its inherited methods; callers go through the captured
// invokers above (a spread would look for a Symbol.iterator that is no longer there).
function bareArray() {
    var a = [];
    _ObjectSetPrototypeOf(a, null);
    return a;
}

// For targets whose prototype cannot be dropped, functions above all: defineProperty
// writes an own property outright where assignment would offer the value to an accessor
// inherited from the unfrozen Function.prototype. This file has no "use strict":
// misdirected assignment fails silently and the planted property wins for good.
function ownProp(target, name, value) {
    _ObjectDefineProperty(target, name, {
        value: value, writable: true, enumerable: true, configurable: true,
    });
}

// ── Mocked process (filtered env, no exit/kill/binding) ─────────────────
const ALLOWED_ENV_KEYS = new Set([
    "TMPDIR", "TMP", "TEMP",
    "HOME", "USERPROFILE", "PATH", "APPDATA",
]);

// A snapshot, not a live view: process.env IS globalThis.Bun.env, the same object that
// hardenBun cannot neuter. A plugin can plant a key there and every plugin
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
// restorePrototypes preserves added properties by design: an inherited entry
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

// A failed write arrives as an `error` event, not as a throw at the call site. A
// try/catch around `.write()` never sees it and an unlistened event ends the process.
// Measured: one broken pipe took the whole child down, and with it every plugin's
// native support. Nothing respawns it. Both streams need this, not just stdout;
// stderr is written from the warning paths and from the stdout handler just below, so
// leaving it bare would kill the process through the very handler meant to save it.
// Nothing constructive is left to do once a pipe is gone: no line can reach Rust.
hostStdout.on("error", function() {
    try { hostStderr.write("[sandbox] stdout write failed\n"); } catch (_) {}
});
hostStderr.on("error", function() {});
// The Object statics live further up, ahead of the env builder that needs them.
var _JSONStringify = JSON.stringify;
var _PromiseReject = Promise.reject.bind(Promise);
// The constructor too, and for a sharper reason than the static: an executor is handed
// `resolve`/`reject`, and ipcFetch stores that pair in `pendingFetches`, a map shared by
// every plugin in this process. Resolving `Promise` at call time lets whoever replaced it
// last receive another plugin's settlement functions.
var _RealPromise = Promise;
// Values, not the object: realFs.constants is handed to plugins on the fs facade and
// is not frozen; reading S_IFSOCK at decision time would read a plugin's number.
var _S_IFMT = realFs.constants.S_IFMT;
var _S_IFSOCK = realFs.constants.S_IFSOCK;
// The access() mode bits fall under that same rule: they pick which gate a probe goes
// through. A live read lets a plugin route a write-mode probe into the read gate.
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
var _ArrayPrototypeUnshift = _uncurryThis(Array.prototype.unshift);
var _ArrayPrototypeSome = _uncurryThis(Array.prototype.some);
var _ArrayFrom = Array.from;
var _StringPrototypeSlice = _uncurryThis(String.prototype.slice);
var _FunctionPrototypeBind = _uncurryThis(Function.prototype.bind);
var _RealFunction = Function;
var _ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
var _SetPrototypeHas = _uncurryThis(Set.prototype.has);
var _SetPrototypeAdd = _uncurryThis(Set.prototype.add);
var _SetPrototypeDelete = _uncurryThis(Set.prototype.delete);
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
// The listener pair, uncurried: `signal` comes FROM the plugin; freezing EventTarget is
// only half of it. An own property on the instance still shadows a frozen prototype. These
// invokers throw on anything that is not a real EventTarget, and both call sites already
// swallow: a forged signal loses its listener instead of being handed the host's closure.
var _AddEventListener = typeof EventTarget !== "undefined"
    ? _uncurryThis(EventTarget.prototype.addEventListener)
    : undefined;
var _RemoveEventListener = typeof EventTarget !== "undefined"
    ? _uncurryThis(EventTarget.prototype.removeEventListener)
    : undefined;

// Nothing else here guards `EventEmitter`/`Writable`/`Readable`: freezeModuleExports cannot
// reach them, freezePrimordials only knows globals, restorePrototypes tracks six others, while
// `events`/`stream` reach every plugin untrusted. Two zero-trust escapes came through that gap:
// a forged IPC `call` into another plugin's module via the shared readline's `emit`, and the raw
// fs WriteStream with its `Symbol(kFs)` handed back through `on`/`once`.
//
// Pinned rather than frozen or captured, both measured. `Object.freeze` breaks every
// construction, this host's own readline included: EventEmitter's constructor shadows an
// inherited default by plain assignment (`this[kCapture] = ...`). Capturing misses the other
// side: Bun's Writable constructor calls `this.once(Symbol(kConstruct))` inside
// createWriteStream. A plugin's assignment then fails silently, as everywhere in this file.
function pinMethod(proto, name) {
    var d = _ObjectGetOwnPropertyDescriptor(proto, name);
    if (!d || !("value" in d)) return;
    _ObjectDefineProperty(proto, name, {
        value: d.value, writable: false, configurable: false, enumerable: d.enumerable,
    });
}
;(function pinStreamMethods() {
    var EE = require("events").EventEmitter;
    var Writable = require("stream").Writable;
    var Readable = require("stream").Readable;
    pinMethod(EE.prototype, "on");
    pinMethod(EE.prototype, "once");
    pinMethod(EE.prototype, "addListener");
    pinMethod(EE.prototype, "emit");
    pinMethod(EE.prototype, "removeListener");
    pinMethod(EE.prototype, "listenerCount");
    pinMethod(Writable.prototype, "write");
    pinMethod(Writable.prototype, "end");
    pinMethod(Writable.prototype, "destroy");
    pinMethod(Writable.prototype, "cork");
    pinMethod(Writable.prototype, "uncork");
    pinMethod(Writable.prototype, "setDefaultEncoding");
    pinMethod(Readable.prototype, "push");
    pinMethod(Readable.prototype, "read");
    pinMethod(Readable.prototype, "isPaused");
})();

// Every line this process writes to Rust is built on a null prototype: JSON.stringify finds
// no INHERITED toJSON to call. Object.prototype is left unfrozen for npm compat,
// restorePrototypes preserves additions, and toJSON has no default to shadow: adding it IS
// the attack, replacing the whole payload of every message afterwards. Pinning it there is
// no answer either; measured, a non-writable inherited data property makes the ordinary
// `obj.toJSON = fn` idiom throw. Only plain objects and arrays are rebuilt: anything else
// keeps its prototype; a plugin class still gets its own toJSON honoured, and those
// prototypes are either frozen here or the plugin's own business. Cycles become null through
// the tracked ancestors; a depth cap would not do, it would EXPAND a cycle by the branching
// factor rather than cut it. Shared non-ancestor references serialize twice, as they did before.
//
// Two trackers, because scanning `path` costs one comparison per ancestor: a chain nested D
// deep pays O(D^2), and a plugin picks D. Measured at 20 000 deep, one return value held the
// single Bun process every plugin shares for 205 ms. Past INERT_SCAN_DEPTH the ancestors move
// into a Set and membership stops tracking depth.
//
// `seen` is a parameter, never per-call state: a frame is in Set mode only if its caller
// handed it one. The Set then lives as long as the subtree that crossed the threshold, while
// a sibling that never goes deep starts over on the array at no cost. A frame that MINTED the
// Set still has `seen === null` itself, and pops the array on the way out because its own
// entry went there. Re-entrancy is what rules per-call state out: a plugin getter read during
// the walk can reach sendCancel through its own AbortController, letting a second ipcLine
// start and finish inside this one, which would clear a shared tracker's ancestors underneath
// it. Order is load-bearing at the mint too: the push precedes the threshold test, and the
// seed loop reads a `path` that already holds the node being entered.
var INERT_SCAN_DEPTH = 32;

function inertValue(value, path, seen) {
    if (value === null || typeof value !== "object") return value;
    if (seen !== null) {
        if (_SetPrototypeHas(seen, value)) return null;
    } else {
        for (var k = 0; k < path.length; k++) if (path[k] === value) return null;
    }
    var isArray = _ArrayIsArray(value);
    if (!isArray) {
        var proto = _ObjectGetPrototypeOf(value);
        if (proto !== _ObjectPrototypeRef && proto !== null) return value;
    }
    var childSeen = seen;
    if (seen !== null) {
        _SetPrototypeAdd(seen, value);
    } else {
        path[path.length] = value;
        if (path.length > INERT_SCAN_DEPTH) {
            childSeen = new Set();
            for (var si = 0; si < path.length; si++) _SetPrototypeAdd(childSeen, path[si]);
        }
    }
    var out;
    if (isArray) {
        // Still an array to Array.isArray and to JSON's array branch (neither consults the
        // prototype chain), so the wire format is unchanged.
        out = bareArray();
        for (var i = 0; i < value.length; i++) out[i] = inertValue(value[i], path, childSeen);
    } else {
        out = _ObjectCreate(null);
        var keys = _ObjectKeys(value);
        for (var j = 0; j < keys.length; j++) {
            out[keys[j]] = inertValue(value[keys[j]], path, childSeen);
        }
    }
    if (seen !== null) _SetPrototypeDelete(seen, value);
    else path.length = path.length - 1;
    return out;
}

// The same hazard as above, applied at every depth rather than only the top level: the trap
// is handed `this`; a planted Array.prototype.toJSON both READ and replaced whichever slot
// it got (another plugin's request headers, its exported names, anything it returned). Adding
// it to _protoTracked does not help either; that snapshot only stores properties that already
// exist, and this one does not.
function ipcLine(fields) {
    // The ancestor stack is bare too: it is written by index; a planted accessor there
    // both read every node on its way past and kept `length` at 0, which made the pop
    // underflow and threw RangeError out of every line this process tried to send.
    return _JSONStringify(inertValue(fields, bareArray(), null)) + "\n";
}

// A plugin picks what it returns and what it throws: it picks what `ipcLine` has to
// serialize, and the space of values that break serialization has no useful bound: a
// BigInt, a Proxy whose ownKeys trap throws, an own toJSON (which the null-prototype
// rebuild does NOT remove, only an inherited one), thirty thousand levels of nesting.
// Rather than chase that space, the response is made total: this line answers when the
// faithful one cannot. A caller always gets an answer and the process never dies for
// want of one. Every operation here is deliberately primitive-only: JSON.stringify on a
// string or a number never consults a prototype, where on an object it would.
var MAX_ERROR_LEN = 4096;

function fallbackLine(id, label) {
    var idJson = (typeof id === "string" || typeof id === "number") ? _JSONStringify(id) : "null";
    return "{\"id\":" + idJson + ",\"error\":" + _JSONStringify(label) + "}\n";
}

// Reads the DESCRIPTOR, never the property: an accessor has no `value`; a poisoned
// getter is not merely caught here, it is never invoked. `String(e)` is gone rather than
// guarded. On an object it runs the plugin's own Symbol.toPrimitive. The one step
// left that can throw is the descriptor read itself against a hostile Proxy trap, and
// that is what the try covers (not the design, one named gap in it).
function safeErrorString(e) {
    try {
        if (typeof e === "string")
            return e.length > MAX_ERROR_LEN ? _StringPrototypeSlice(e, 0, MAX_ERROR_LEN) : e;
        if (typeof e === "number" || typeof e === "boolean") return String(e);
        if (e === null || typeof e !== "object") return "[sandbox] non-string error";
        var msg = "";
        var stack = "";
        var msgDesc = _ObjectGetOwnPropertyDescriptor(e, "message");
        if (msgDesc && typeof msgDesc.value === "string") msg = msgDesc.value;
        var stackDesc = _ObjectGetOwnPropertyDescriptor(e, "stack");
        if (stackDesc && typeof stackDesc.value === "string") stack = stackDesc.value;
        if (!msg && !stack) return "[sandbox] non-string error";
        var combined = stack ? msg + "\n" + stack : msg;
        return combined.length > MAX_ERROR_LEN
            ? _StringPrototypeSlice(combined, 0, MAX_ERROR_LEN) : combined;
    } catch (_) {
        return "[sandbox] error normalization failed";
    }
}

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
    // `prototype` is the one name a function already owns (it takes an assignment); the
    // rest are new names on an object whose prototype every plugin can reach.
    FakeModule.prototype = {};
    ownProp(FakeModule, "builtinModules", _ObjectFreeze([]));
    ownProp(FakeModule, "_cache", {});
    ownProp(FakeModule, "_extensions", {});
    ownProp(FakeModule, "createRequire", function() {
        var msg = "[sandbox] module.createRequire is not available";
        try { hostStderr.write(msg + "\n"); } catch (_) {}
        throw new Error(msg);
    });
    ownProp(FakeModule, "syncBuiltinESMExports", function() {});
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
        // stdout and stderr are answered with the inert stub below rather than listed here:
        // the engine's own stream code reads them (a throw denied more than it protected).
        "stdin",
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
            // Bun's own Readable.prototype.pipe compares its destination against
            // process.stdout to decide whether to end it. Throwing here made
            // `readable.pipe(dest)` fail for every plugin on every destination. The stub
            // answers that comparison while being nobody's stdout: it must stay mockedStdio;
            // `target.stdout` is the live fd 1 carrying host replies, and handing it over is
            // the IPC forgery this whole block exists to prevent. Same object the plugin's own
            // shadowed `process` already exposes (this widens nothing).
            if (prop === "stdout") return mockedStdio.stdout;
            if (prop === "stderr") return mockedStdio.stderr;
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
    // Asks the set, not the object. `trustedModules.has(id)` answers with whatever method
    // the object in hand carries, and this gate decides whether a plugin is handed the real
    // `net`/`http`/`fs`; `reachesLocalEndpoint` already reads it this way, and the gap
    // between the two forms is what let a rebound global answer for the store.
    function isTrusted(id, canonical) {
        return _SetPrototypeHas(trustedModules, id)
            || _SetPrototypeHas(trustedModules, canonical);
    }
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
                if (isTrusted(id, canonical))
                    return shimmedWorkerThreads;
                throw new Error("TRUST_REQUIRED:" + canonical);
            }
            // fs/fs-promises: return sandboxed facade instead of real module
            if (canonical === "fs" && sandboxedFs) {
                if (isTrusted(id, canonical))
                    return sandboxedFs;
                throw new Error("TRUST_REQUIRED:" + canonical);
            }
            if (canonical === "fs/promises" && sandboxedFs) {
                if (_SetPrototypeHas(trustedModules, "fs")
                    || _SetPrototypeHas(trustedModules, "fs/promises")
                    || isTrusted(id, canonical))
                    return sandboxedFs.promises;
                throw new Error("TRUST_REQUIRED:fs");
            }
            if (isTrusted(id, canonical)) {
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
        // Reflect.apply, not RealFunction.apply: the latter reads `apply` off a
        // Function.prototype this file deliberately leaves unfrozen, and a poisoned one
        // receives `this === RealFunction`, the real unwrapped constructor, handed to the
        // plugin after the scan above already ran. Reflect is frozen and name-pinned, and
        // its apply invokes [[Call]] directly instead of reading a property off the target.
        return Reflect.apply(RealFunction, this, arguments);
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
    // Safe to fully freeze (no lazy mutation by stdlib after pre-load). Held as NAMES, not
    // values, because one list has to drive two operations that must never disagree: what
    // gets frozen, and what gets pinned to its name. The web tier at the end arrived with
    // the fetch path: the body encoder reads members off those while one plugin's request
    // is built from a value another plugin supplied, including the Blob name/type getters
    // that land inside a multipart header.
    var fullFreeze = [
        "URL", "Map", "Set", "WeakMap", "WeakSet", "RegExp", "Date",
        "JSON", "Math", "Reflect",
        "Int8Array", "Uint8Array", "Int16Array", "Uint16Array",
        "Int32Array", "Uint32Array", "Float32Array", "Float64Array",
        "BigInt64Array", "BigUint64Array", "Symbol",
        "Request", "Headers", "Response", "FormData", "Blob", "URLSearchParams",
        // The views above were listed, their backing buffer was not: ArrayBuffer carries no
        // own Symbol.hasInstance. One was freely definable, and such a trap receives the
        // object under test: a read channel on another plugin's request body while it
        // answers true and nothing looks wrong. And Buffer: freezing the `buffer` module's
        // exports is shallow. Buffer.from and Buffer.prototype.toString stayed writable
        // while every body encode reads them. Freezing Uint8Array does not cover either:
        // both are Buffer's OWN properties, not inherited.
        "ArrayBuffer", "Buffer",
        // The abort tier. A fetch hands its own `onAbort` closure to
        // signal.addEventListener, and that closure already carries the reqId; whoever
        // intercepts the registration can settle and cancel a request belonging to someone
        // else. Freezing closes the shared-prototype half; the instance half needs the
        // captured invokers above, because the signal itself comes from the plugin.
        "EventTarget", "AbortSignal", "AbortController",
    ];
    _ArrayPrototypeForEach(fullFreeze, function(name) {
        var obj = globalThis[name];
        if (obj === undefined || obj === null) return;
        try { _ObjectFreeze(obj); } catch (_) {}
        try { if (obj.prototype) _ObjectFreeze(obj.prototype); } catch (_) {}
        // Pinning the NAME is the other half, and it is the half that was missing. Freezing
        // the object stops its own properties from moving; nothing stopped
        // `globalThis.Set = class extends Set { has() { return true; } }`, after which host
        // code doing `new Set()` builds a store the plugin controls, which is how a trust
        // set and an fs grant store came to answer a plugin's questions with its own answers.
        try {
            _ObjectDefineProperty(globalThis, name, {
                value: obj, writable: false, configurable: false,
            });
        } catch (_) {}
    });

    // Built-in prototypes (Object, Array, Function, String, Promise, Error, etc.)
    // are NOT frozen - npm packages bundled into plugins assign to inherited
    // property names (e.g. node-inspect-extracted), which throws in strict mode
    // when the prototype is frozen. The shared-module mutation vector is covered
    // by freezeSafeModuleExports() below instead.

    // Those same six still get their NAME pinned, which is a different question from the
    // one the note above answers. Freezing a prototype and rebinding a global are separate
    // powers, and only the first carries the npm cost: pinning stops
    // `globalThis.String = f` and nothing else. `String.prototype.x = y` and
    // `String.raw = f` both still work, measured.
    // Not cosmetic. `String(...)` runs at six decision-time sites and `new Error(...)` at
    // thirty (every gate in this file) with no captured fallback. A rebound String let
    // one plugin rewrite the URL of another's fetch, carrying its Authorization header and
    // its cookie jar to a host of the attacker's choosing.
    // `Function` is absent because hardenFunction already pinned it, to SafeFunction.
    // Known cost, measured and accepted: zone.js assigns `globalThis.Promise`
    // unconditionally and would now throw. It is Angular tooling and nothing on this path
    // bundles it. A plugin writing the legacy `global.Promise = require('bluebird')` line
    // fails the same way: silently while sloppy, loudly under "use strict".
    _ArrayPrototypeForEach(["Object", "Array", "String", "Promise", "Error"], function(name) {
        try {
            _ObjectDefineProperty(globalThis, name, {
                value: globalThis[name], writable: false, configurable: false,
            });
        } catch (_) {}
    });
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
                // Functions too: Bun exports `events`, `stream`, `assert` and `assert/strict`
                // as callables, and testing only for "object" skipped all four in silence.
                // Freezing the module object leaves its `.prototype` writable, which is why
                // the stream/emitter methods are pinned by name above; this half only stops
                // `mod.Foo = evil`.
                if (mod && (typeof mod === "object" || typeof mod === "function")) {
                    _ObjectFreeze(mod);
                }
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

// The read-side twin of `ownProp`, against the same hazard from the other direction.
// Anything a plugin hands in inherits whatever another plugin left on Object.prototype or
// Array.prototype, both deliberately unfrozen for npm compat, and the ordinary idioms
// `if (opts.method)` and `if (h[i])` make the presence test and the read the SAME
// operation (the inherited value is already in hand before anything could reject it).
// A hole in an array reads through exactly the same way an absent key does.
// Never `hasOwnProperty.call`: that reads `.call` off the unfrozen Function.prototype at
// decision time, which is the hazard rather than the guard.
function ownRead(obj, name) {
    return _ObjectGetOwnPropertyDescriptor(obj, name) === undefined ? undefined : obj[name];
}

function normalizeHeaders(h) {
    // Ordered [name, value] pairs: preserves duplicate header names and avoids the
    // __proto__ hazard of a plain object. Headers tested first (a Map's forEach differs).
    var out = bareArray();
    if (!h) return out;
    if (_RealHeaders && h instanceof _RealHeaders) {
        h.forEach(function(v, k) { _ArrayPrototypePush(out, [k, String(v)]); });
    } else if (_ArrayIsArray(h)) {
        for (var i = 0; i < h.length; i++) {
            // The pair itself can be sparse too: both levels are read as own or not
            // at all; a forged pair reaches the wire otherwise.
            var pair = ownRead(h, i);
            if (!pair) continue;
            var pName = ownRead(pair, 0);
            var pValue = ownRead(pair, 1);
            _ArrayPrototypePush(out, [String(pName), String(pValue)]);
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
    var entries = bareArray();
    fd.forEach(function(value, key) { _ArrayPrototypePush(entries, [key, value]); });
    var chunks = bareArray();
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
        try { _RemoveEventListener(entry.signal, "abort", entry.onAbort); } catch (_) {}
    }
    return entry;
}

// Tell Rust to drop an in-flight net.fetch (the child gave up: abort or timeout);
// it stops doing network work nobody awaits.
function sendCancel(reqId) {
    try {
        hostStdout.write(ipcLine({ type: "net.fetch.cancel", reqId: reqId }));
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
        } else if (input && typeof input === "object" && typeof ownRead(input, "url") === "string") {
            // Every field here is read as own or not at all. A plain `{url}` object carries
            // none of the others in its own right; the plain reads walked to
            // Object.prototype, and one plugin setting `Object.prototype.method` there
            // rewrote the outbound request of every OTHER plugin, with no trust of any kind.
            url = ownRead(input, "url");
            var inMethod = ownRead(input, "method");
            if (inMethod) method = inMethod;
            var inHeaders = ownRead(input, "headers");
            if (inHeaders) headers = normalizeHeaders(inHeaders);
        } else {
            url = String(input);
        }
        if (init) {
            var itMethod = ownRead(init, "method");
            if (itMethod) method = itMethod;
            var itHeaders = ownRead(init, "headers");
            if (itHeaders) headers = normalizeHeaders(itHeaders);
            var itBody = ownRead(init, "body");
            if (itBody != null) body = itBody;
            var itRedirect = ownRead(init, "redirect");
            if (itRedirect) redirect = itRedirect;
            var itSignal = ownRead(init, "signal");
            if (itSignal !== undefined) signal = itSignal;
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

        return await new _RealPromise(function(resolve, reject) {
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
                try { _AddEventListener(signal, "abort", onAbort); } catch (_) {}
            }
            pendingFetches.set(reqId, {
                resolve: resolve, reject: reject, timer: timer, signal: signal, onAbort: onAbort,
            });
            try {
                hostStdout.write(ipcLine({
                    type: "net.fetch", reqId: reqId, plugin: pluginName,
                    url: url, method: method, headers: headers, body: enc.b64,
                    redirect: redirect || "follow",
                }));
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
        // The dispatcher calls this one BEFORE entering its try: a throw here has no
        // handler anywhere. `new TypeError(x)` coerces x, and coercing a nested array runs
        // Array.prototype.toString down its whole depth: measured, that overflows the
        // stack and ends the process. Rust only sends a string here today; accepting only
        // a string is what keeps that from being load-bearing.
        var why = typeof cmd.error === "string" ? cmd.error : "fetch failed";
        entry.reject(new _RealTypeError(why));
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
// `..` is still resolved away; containment cannot be escaped, but a symlink keeps
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
    var parts = bareArray();
    var current = resolved;
    while (true) {
        var real;
        // Only a realpathSync throw means "this ancestor does not exist yet". The join used
        // to share this catch. Anything it threw promoted an existing ancestor to a
        // missing one, the walk ran on to the root, and the fallthrough handed the gate the
        // caller's raw lexical path with its symlinks unresolved, authorizing a write
        // outside the dataDir that isInDirs then read as contained.
        try {
            real = realFs.realpathSync(current);
        } catch (_) {
            _ArrayPrototypeUnshift(parts, pathMod.basename(current));
            var parent = pathMod.dirname(current);
            if (parent === current) break;
            current = parent;
            continue;
        }
        var args = bareArray();
        args[0] = real;
        for (var i = 0; i < parts.length; i++) args[i + 1] = parts[i];
        // Reflect.apply reads length and indices where a spread would want an iterator,
        // the same reason the call handler below avoids one.
        return Reflect.apply(pathMod.join, pathMod, args);
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
    if (_SetPrototypeHas(grants.readFiles, real)) return true;
    if (_SetPrototypeHas(grants.writeFiles, real)) return true;
    return isInDirs(real, _ArrayFrom(grants.dirs));
}

// Every gate below returns the path it authorized, and its caller operates on that one.
// Handing back nothing left each facade method to re-derive a target from the argument,
// which is a second path: canonicalization is not the identity. The two disagree
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
    if (_SetPrototypeHas(grants.writeFiles, real)) return real;
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

// The write-stream surface Node documents, minus the two that are capabilities rather than
// settings. `fd` makes Node ignore the path entirely, and `fs` hands it functions to call.
var WRITE_STREAM_OPTS = [
    "flags", "encoding", "mode", "autoClose", "emitClose", "start", "highWaterMark",
    "signal", "flush",
];

// Every name the WriteStream/Writable/WritableState/EventEmitter constructors read BY NAME,
// measured by reading those constructors on Bun and on Node, not the names a caller may pass.
var ENGINE_STREAM_OPT_NAMES = [
    "fd", "fs", "flags", "mode", "start", "flush", "encoding", "autoClose",
    "objectMode", "highWaterMark", "emitClose", "defaultEncoding", "autoDestroy",
    "destroy", "final", "construct", "write", "writev", "signal",
    "captureRejections", "decodeStrings", "path",
];

// Returns the options it authorized, and the caller opens with THOSE, the same rule the path
// gates above follow. Refusing was not enough: `'fd' in opts` is a [[HasProperty]] check while
// Node finds the option by enumeration; a Proxy whose `has` trap lied and whose ownKeys told
// the truth passed the gate and still supplied a descriptor. Naming only the keys we allow
// makes that smuggle inexpressible.
//
// Sanitizing what we hand over is half the job: the engine re-copies our keys onto a plain
// object of ITS OWN (`copyObject`) and reads the options off that, where any name absent from
// ours resolves up to whatever a plugin left on Object.prototype: measured, a handed-over
// `fd`, an `fs` taking every write, a `construct` that hangs the process. Every name is
// therefore set OWN, `undefined` where nothing authorized it; both runtimes read that as absent.
//
// No shape leaves unsaturated. `undefined`, `null` and the bare string are the shapes plugins
// actually use, and passing one through untouched let the engine build the object itself.
function authorizedWriteStreamOpts(opts) {
    var bareEncoding = typeof opts === "string" ? opts : undefined;
    var carried = opts !== null && opts !== undefined && typeof opts === "object";
    // No prototype on `out`: an inherited setter would take the assignment below, and no own
    // property would land to shadow the engine's read.
    var out = _ObjectCreate(null);
    _ArrayPrototypeForEach(ENGINE_STREAM_OPT_NAMES, function(key) {
        if (key === "encoding" && bareEncoding !== undefined) {
            out[key] = bareEncoding;
        } else if (carried && _ArrayPrototypeIndexOf(WRITE_STREAM_OPTS, key) !== -1) {
            // A plugin `get` trap fires here, once, and it is this copy the engine receives:
            // `ownRead` still reads through an OWN accessor: that stays true.
            // Own-only because the paragraph above is only half the job. Saturating `out` keeps
            // the ENGINE off Object.prototype; this read is the same hazard on the way IN, and
            // an option its caller never passed (measured with `mode`) otherwise resolves
            // to whatever another plugin left on the prototype every plugin shares.
            out[key] = ownRead(opts, key);
        } else {
            out[key] = undefined;
        }
    });
    return out;
}

// The outcome test: it catches an `fd` that reached the engine anyway, and reads an OWN
// descriptor because a plain read of `stream.path` answers from an inherited getter just as
// happily. A refused stream is left open rather than destroyed: closing it would rest on the
// autoClose default for an fd-bound stream, and being wrong there shuts our own pipe.
function openAuthorizedWriteStream(real, opts) {
    var stream = realFs.createWriteStream(real, authorizedWriteStreamOpts(opts));
    if (_ObjectGetOwnPropertyDescriptor(stream, "path") && stream.path === real) return stream;
    throw new Error("[sandbox] fs write stream did not open on the authorized path: " + real);
}

// Bun's Writable constructor seeds its handler store with exactly these names: subscribing
// to one of them finds an own property and never reads through to Object.prototype. Measured:
// `_events` owns close/error/prefinish/finish/drain on a fresh stream, and subscribing to any
// name it does NOT own makes the engine's own `handlers.push` fail against whatever a plugin
// left on the prototype. Subscribing to only these at construction is what keeps one poisoned
// name from denying every write stream in the process.
var WRITE_STREAM_SEEDED_EVENTS = ["error", "finish", "close", "drain"];
// The rest read through: they are subscribed to only when a plugin asks for one. The
// exposure then belongs to the caller that wanted it rather than to every caller, and it
// refuses by name instead of going quiet.
var WRITE_STREAM_LAZY_EVENTS = ["open", "ready", "pipe", "unpipe"];

// The one method in this whole facade that would otherwise hand back a live engine object, and
// that object carries an own Symbol(kFs) holding the REAL fs module. `getOwnPropertySymbols` is
// public (a Symbol is lexically private, not access-controlled), so the plugin reads it and
// every gate in this file is out of the picture. Measured: a read and a write outside the
// dataDir, in the same call where the plugin's own sandboxed readFileSync was denied.
//
// A wrapper owns its OWN listener registry, and that is the load-bearing part. EventEmitter
// invokes a listener with `this` bound to the emitter it was registered on: forwarding
// `real.on(cb)` would hand the raw stream back as `this` on the plugin's first 'finish'
// handler. No Proxy can close that: registration already happened on the real object, and the
// traps are never consulted again. Measured, both ways round.
// Same reason every chainable method returns the wrapper: Writable returns `this`, so
// forwarding the return value leaks the stream through the ordinary `.on(...).destroy()` idiom.
// Not built on EventEmitter deliberately: its prototype is neither frozen nor tracked by
// restorePrototypes; extending it would add a consumer of an unguarded shared prototype.
function wrapWriteStream(real) {
    var listeners = _ObjectCreate(null);
    var wrapper = _ObjectCreate(null);
    // Read once, here: a per-call `real.write` would re-read a property the engine's own
    // prototype exposes, and this way the wrapper's behaviour is fixed at construction.
    var realWrite = real.write;
    var realEnd = real.end;
    var realDestroy = real.destroy;
    var realCork = real.cork;
    var realUncork = real.uncork;
    var realSetDefaultEncoding = real.setDefaultEncoding;

    function slotFor(ev) {
        if (listeners[ev] === undefined) listeners[ev] = bareArray();
        return listeners[ev];
    }

    function addListener(ev, fn, once, front) {
        if (typeof fn !== "function") return wrapper;
        if (_ArrayPrototypeIndexOf(WRITE_STREAM_LAZY_EVENTS, ev) !== -1) relay(ev);
        var list = slotFor(ev);
        var entry = _ObjectCreate(null);
        entry.fn = fn;
        entry.once = !!once;
        if (front) {
            for (var i = list.length; i > 0; i--) list[i] = list[i - 1];
            list[0] = entry;
        } else {
            list[list.length] = entry;
        }
        return wrapper;
    }

    function dispatch(ev, args) {
        var list = listeners[ev];
        if (list === undefined) return false;
        // Snapshot: a listener is free to add or remove listeners while it runs.
        var live = bareArray();
        for (var i = 0; i < list.length; i++) live[i] = list[i];
        for (var j = 0; j < live.length; j++) {
            var entry = live[j];
            if (entry.once) removeEntry(ev, entry.fn);
            // Contained on purpose, and this diverges from Node: there, a throwing listener
            // reaches the top and ends the process, which here would be one plugin ending
            // every plugin's native support. The throw belongs to whoever registered it.
            try { Reflect.apply(entry.fn, wrapper, args); }
            catch (e) { try { hostStderr.write("[sandbox] write stream listener threw: " + safeErrorString(e) + "\n"); } catch (_) {} }
        }
        return live.length > 0;
    }

    function removeEntry(ev, fn) {
        var list = listeners[ev];
        if (list === undefined) return wrapper;
        for (var i = 0; i < list.length; i++) {
            if (list[i].fn !== fn) continue;
            for (var j = i; j < list.length - 1; j++) list[j] = list[j + 1];
            list.length = list.length - 1;
            break;
        }
        return wrapper;
    }

    // Relays read `arguments` and never `this` (nothing they hand on can be the raw stream).
    // Subscribing for 'error' also means the engine never sees that event as unhandled, which
    // on its own would have ended the process.
    var relayed = _ObjectCreate(null);
    function relay(ev) {
        if (relayed[ev]) return;
        try { real.on(ev, function() { dispatch(ev, arguments); }); }
        catch (_) {
            // Only the lazy names can land here, and only because a plugin left something under
            // that name on Object.prototype. Refusing names the cause at the call that asked
            // for it; going quiet instead would leave that plugin waiting on an event forever.
            throw new Error("[sandbox] fs write stream cannot report '" + ev
                + "': Object.prototype." + ev + " is set");
        }
        relayed[ev] = true;
    }
    _ArrayPrototypeForEach(WRITE_STREAM_SEEDED_EVENTS, relay);

    ownProp(wrapper, "write", function() { return Reflect.apply(realWrite, real, arguments); });
    ownProp(wrapper, "end", function() { Reflect.apply(realEnd, real, arguments); return wrapper; });
    ownProp(wrapper, "destroy", function() { Reflect.apply(realDestroy, real, arguments); return wrapper; });
    ownProp(wrapper, "cork", function() { Reflect.apply(realCork, real, arguments); });
    ownProp(wrapper, "uncork", function() { Reflect.apply(realUncork, real, arguments); });
    ownProp(wrapper, "setDefaultEncoding", function(enc) {
        Reflect.apply(realSetDefaultEncoding, real, [enc]);
        return wrapper;
    });
    ownProp(wrapper, "on", function(ev, fn) { return addListener(ev, fn, false, false); });
    ownProp(wrapper, "addListener", function(ev, fn) { return addListener(ev, fn, false, false); });
    ownProp(wrapper, "once", function(ev, fn) { return addListener(ev, fn, true, false); });
    ownProp(wrapper, "prependListener", function(ev, fn) { return addListener(ev, fn, false, true); });
    ownProp(wrapper, "prependOnceListener", function(ev, fn) { return addListener(ev, fn, true, true); });
    ownProp(wrapper, "off", function(ev, fn) { return removeEntry(ev, fn); });
    ownProp(wrapper, "removeListener", function(ev, fn) { return removeEntry(ev, fn); });
    ownProp(wrapper, "emit", function(ev) {
        var args = bareArray();
        for (var i = 1; i < arguments.length; i++) args[i - 1] = arguments[i];
        return dispatch(ev, args);
    });

    // Primitives only, read off `real` at call time. A getter that returned `real` itself, or
    // anything holding it, would reopen the whole thing.
    _ArrayPrototypeForEach(
        ["fd", "path", "bytesWritten", "writable", "writableEnded", "writableFinished",
            "writableNeedDrain", "writableHighWaterMark", "writableLength", "destroyed", "closed"],
        function(name) {
            _ObjectDefineProperty(wrapper, name, {
                get: function() { return real[name]; }, enumerable: true, configurable: false,
            });
        });

    return _ObjectFreeze(wrapper);
}

// Where a local IPC endpoint can live: the session runtime dir the register command
// carried, plus `hostTmpDir`, the OS temp dir this process resolves for itself, not the
// directory the parent started it in. Those are two independent answers: the parent picks
// a cwd, this one reads the env, and the two precedences differ (Rust consults TMPDIR
// alone, os.tmpdir() falls through TMPDIR, TMP, TEMP); agreement is a coincidence of
// the usual environment rather than something either side guarantees. Built per plugin,
// and an empty list is how a plugin without endpoint trust is expressed: there is no
// separate flag to disagree with it.
function probeDirsFor(runtimeDir) {
    var out = bareArray();
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
        // that resolve are added: this is the same disclosed dir by two spellings.
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
// socket (lstat would answer about the link instead).
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
            // Real existsSync answers false on every error, EACCES included: a
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
            return wrapWriteStream(openAuthorizedWriteStream(checkWrite(p), o));
        },
        constants: _fsConstantsFrozen,
    };

    // Callers feature-detect .native before using it (typescript, resolve); a facade
    // without one fails their check and takes a path they only meant as a fallback.
    ownProp(facade.realpathSync, "native",
        gatedRealpath(realFs.realpathSync.native || realFs.realpathSync));

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
        // Every gate throws, but this facade is awaited: each arm owes a REJECTED promise
        // and must never throw at the call site, or a caller that chains `.catch()` rather
        // than wrapping in try/catch never sees the denial at all. The conversion uses the
        // captured reject. A swapped `Promise.reject` cannot turn a denial into a
        // resolved promise. The gates are called inside the try on purpose: as an argument
        // expression they run before any promise exists, which is what threw synchronously.
        realpath: function(p, o) {
            try { return gatedRealpath(p, o); } catch (e) { return _PromiseReject(e); }
        },
        readFile: function(p, o) {
            try { return realFs.promises.readFile(checkRead(p), o); }
            catch (e) { return _PromiseReject(e); }
        },
        writeFile: function(p, d, o) {
            try { return realFs.promises.writeFile(checkWrite(p), d, o); }
            catch (e) { return _PromiseReject(e); }
        },
        mkdir: function(p, o) {
            try { return realFs.promises.mkdir(checkMkdir(p), o); }
            catch (e) { return _PromiseReject(e); }
        },
        stat: function(p, o) {
            try { return realFs.promises.stat(checkRead(p), o); }
            catch (e) { return _PromiseReject(e); }
        },
        unlink: function(p) {
            try { return realFs.promises.unlink(checkDelete(p)); }
            catch (e) { return _PromiseReject(e); }
        },
        rm: function(p, o) {
            try { return realFs.promises.rm(checkDelete(p), o); }
            catch (e) { return _PromiseReject(e); }
        },
        access: function(p, mode) {
            var m = (mode === undefined) ? _F_OK : mode;
            if (m & _X_OK)
                return _PromiseReject(new Error("[sandbox] fs X_OK denied"));
            try {
                var gated = (m & _W_OK) ? checkWrite(p) : checkRead(p);
                return realFs.promises.access(gated, mode);
            } catch (e) { return _PromiseReject(e); }
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
// Name -> exports, with no prototype: an accessor planted under a plugin's exact module
// name swallowed the registration (the ack still reported the real exports) and then
// answered every later call with the attacker's function, holding the real arguments.
const modules = _ObjectCreate(null);

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
            // keeps added properties. A plugin planting Object.prototype.fs = true
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
            // `module.exports` is an ordinary object: a name the module never exported
            // resolves up its chain. A plugin that plants a function at that name on
            // Object.prototype has it run here and its return value answered as if the
            // module had exported it. Rust's channel token keeps the caller on its own
            // module today. The reach is self-inflicted, but the read is wrong on its
            // own terms, and the token is not this file's invariant to lean on.
            var member = ownRead(mod, fnName);
            if (typeof member === "function") {
                // Reflect.apply rather than a spread: spreading reads
                // Array.prototype[Symbol.iterator], and _protoTracked lists string keys
                // only; a poisoned iterator is never restored, not even by a later
                // register, and this line runs for every call of every plugin afterwards.
                // Reflect.apply reads length and indices instead of iterating.
                var callArgs = _ArrayIsArray(args) ? args : [];
                var result = await Reflect.apply(member, undefined, callArgs);
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
        respondError(id, e);
    }
});

function respond(id, data) {
    var line;
    try { line = ipcLine({ id, ...data }); }
    catch (e) { line = fallbackLine(id, "[sandbox] response serialization failed: " + safeErrorString(e)); }
    hostStdout.write(line);
}

// Takes the error raw and normalizes it here: no caller has to know that reading
// `.message` off a thrown value runs code the plugin wrote.
function respondError(id, error) {
    var line;
    try { line = ipcLine({ id, error: safeErrorString(error) }); }
    catch (e) { line = fallbackLine(id, "[sandbox] response serialization failed: " + safeErrorString(e)); }
    hostStdout.write(line);
}
