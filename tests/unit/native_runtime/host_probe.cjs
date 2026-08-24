// Policy probe for frontend/scripts/native-host.cjs, driven by host_script.rs.
//
// The script under test hardens its own realm and has no exports; it is loaded
// exactly the way it loads a plugin - through new Function - and the assertions are
// appended into that same scope. Anything short of this tests a copy, not the file
// the binary embeds.
//
// No data is interpolated into the generated body: fixtures arrive as an extra
// parameter; the only concatenated strings are the host source and this file's
// own assertions.
//
// Every value the policy depends on comes from a fixture, never from the ambient
// environment: an assertion that reads the machine's own XDG_RUNTIME_DIR passes on
// undefined === undefined wherever the session has none.
//
// Usage: bun host_probe.cjs <path to native-host.cjs>

const fs = require("fs");
const path = require("path");
const os = require("os");
const net = require("net");

const HOST = process.argv[2];
if (!HOST) {
    process.stderr.write("usage: bun host_probe.cjs <path to native-host.cjs>\n");
    process.exit(2);
}
const src = fs.readFileSync(HOST, "utf8");

// Fixtures come first: loading the host freezes the realm.
const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "luna-host-probe-")));
const dataDir = path.join(scratch, "data");
fs.mkdirSync(dataDir);
fs.writeFileSync(path.join(dataDir, "inside.txt"), "x");

const outsideFile = path.join(scratch, "outside.txt");
fs.writeFileSync(outsideFile, "x");

// A stand-in for the session runtime dir: the value under test is ours and the
// assertions mean the same thing on a machine that has no such dir at all.
const runtimeDir = path.join(scratch, "run");
fs.mkdirSync(runtimeDir);
// A plain file inside a disclosed dir: what a corrupted socket test would expose.
const runtimePlain = path.join(runtimeDir, "plain.txt");
fs.writeFileSync(runtimePlain, "x");

// Named pipes are not filesystem sockets; the socket cases only exist on unix.
const onUnix = process.platform !== "win32";
const insideSock = path.join(runtimeDir, "endpoint.sock");
const outsideSock = path.join(scratch, "stray.sock");
// A bridged endpoint: a symlink under the disclosed dir whose target is a socket
// elsewhere (the SSH/Flatpak case). Created below once outsideSock exists.
const bridgedSock = path.join(runtimeDir, "bridged.sock");

// A disclosed dir reached through a symlinked prefix (macOS /var -> /private/var): the
// lexical name and its realpath differ, and a caller may hand existsSync either one.
// Symlinks need privilege on Windows: this fixture is unix-only.
const aliasReal = path.join(scratch, "aliasreal");
const aliasLink = onUnix ? path.join(scratch, "aliaslink") : null;
if (onUnix) {
    fs.mkdirSync(aliasReal);
    fs.symlinkSync(aliasReal, aliasLink);
}

const ASSERTIONS = `
;(function probe() {
    var out = [];
    var failed = false;
    // native-host.cjs registers rl.on("close", () => exit(0)) for the real runtime. Under
    // the Rust driver stdin is null: EOF is immediate, and its exit(0) would beat
    // any failure this probe wants to report. Drop it before anything else runs.
    try { rl.removeAllListeners("close"); } catch (_) {}
    // JSON.stringify renders a function, a symbol AND undefined all as the value undefined,
    // so comparing its output made those three indistinguishable, and one assertion here
    // guards exactly that difference: a stolen abort closure is a function where undefined
    // is expected, and it read as PASS. NaN and null both render as "null" for the same
    // reason. Naming the type first keeps the comparison honest.
    function show(v) {
        if (v === undefined) return "undefined";
        if (typeof v === "function") return "[function]";
        if (typeof v === "symbol") return "[symbol]";
        if (typeof v === "number" && v !== v) return "[NaN]";
        if (typeof v === "bigint") return "[bigint " + v + "]";
        try { return JSON.stringify(v); } catch (e) { return "[unserializable " + e.message + "]"; }
    }
    function t(name, got, want) {
        var ok = show(got) === show(want);
        if (!ok) failed = true;
        out.push((ok ? "PASS  " : "FAIL  ") + name + "  got=" + show(got));
    }
    function threw(name, fn, needle) {
        try {
            fn();
            failed = true;
            out.push("FAIL  " + name + "  returned instead of throwing");
        } catch (e) {
            var ok = String(e.message).indexOf(needle) !== -1;
            if (!ok) failed = true;
            out.push((ok ? "PASS  " : "FAIL  ") + name + "  " + e.message);
        }
    }
    function keysOf(o) { return Object.keys(o).sort(); }
    // A rejection left unhandled is noise, but a forged denial is a plain object with no
    // catch: guarding keeps the report, which names the site, instead of a bare TypeError.
    function settle(v) { if (v && typeof v.catch === "function") v.catch(function() {}); }

    var F = probeFixtures;
    var grants = { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() };
    var TMP = realFs.realpathSync(require("os").tmpdir());
    // Taken before any plugin in this script has run: it is the genuine constructor to
    // compare the host's capture against.
    var realPromiseCtor = Promise;
    var realSetCtor = Set;
    var realStringCtor = String;

    // The body is deliberately left at its original indentation: re-indenting hundreds of lines
    // of security assertions to wrap them would make the diff unreviewable.
    try {

    // ── The policy decision itself, not just its effect ──────────────────
    t("net reaches an endpoint", reachesLocalEndpoint(new Set(["net"])), true);
    t("http reaches one too", reachesLocalEndpoint(new Set(["http"])), true);
    t("https as well", reachesLocalEndpoint(new Set(["https"])), true);
    t("fs alone does not", reachesLocalEndpoint(new Set(["fs"])), false);
    t("no trust does not", reachesLocalEndpoint(new Set()), false);

    t("probe dirs carry the given runtime dir",
        _ArrayPrototypeIndexOf(probeDirsFor(F.runtimeDir), F.runtimeDir) !== -1, true);
    t("probe dirs drop one that does not exist",
        _ArrayPrototypeIndexOf(probeDirsFor(F.scratch + "/nope"), F.scratch + "/nope"), -1);
    // Membership, not exact equality: on an aliased tmpdir (macOS TMPDIR under /var,
    // realpath under /private/var) probeDirsFor returns both forms (the canonical TMP
    // is present alongside its lexical alias rather than being the sole entry).
    t("probe dirs keep the temp dir with no runtime dir",
        _ArrayPrototypeIndexOf(probeDirsFor(null), TMP) !== -1, true);

    // A dir reached through a symlinked prefix is stored under BOTH names; existsSync
    // matches whether the caller passes the realpath or the lexical (symlinked) form -
    // getTempDir hands the realpath, a plugin reading env TMPDIR hands the lexical one.
    if (F.aliasLink) {
        var aliasDirs = probeDirsFor(F.aliasLink);
        t("symlinked-prefix dir carries its canonical form",
            _ArrayPrototypeIndexOf(aliasDirs, F.aliasReal) !== -1, true);
        t("symlinked-prefix dir carries its lexical alias",
            _ArrayPrototypeIndexOf(aliasDirs, F.aliasLink) !== -1, true);
        t("the two forms really differ (the symlink is live)", F.aliasReal !== F.aliasLink, true);
    }

    // ── The env door ─────────────────────────────────────────────────────
    var trustedEnv = endpointEnvFor(new Set(["net"]), F.runtimeDir).env;
    var plainEnv = endpointEnvFor(new Set(["fs"]), F.runtimeDir).env;
    t("endpoint trust carries the runtime dir", trustedEnv.XDG_RUNTIME_DIR, F.runtimeDir);
    t("fs-only trust does not", plainEnv.XDG_RUNTIME_DIR, undefined);
    t("no runtime dir means no key",
        endpointEnvFor(new Set(["net"]), null).env.XDG_RUNTIME_DIR, undefined);
    t("exactly one key is added", keysOf(trustedEnv).filter(function(k) {
        return _ArrayPrototypeIndexOf(keysOf(plainEnv), k) === -1;
    }), ["XDG_RUNTIME_DIR"]);
    t("the env has no prototype", Object.getPrototypeOf(trustedEnv), null);
    t("the env still answers hasOwnProperty",
        trustedEnv.hasOwnProperty("XDG_RUNTIME_DIR"), true);
    t("and hasOwnProperty is not one of its keys",
        _ArrayPrototypeIndexOf(keysOf(trustedEnv), "hasOwnProperty"), -1);

    // ── The facade, gated by the dir list alone ──────────────────────────
    var netTrusted = makeSandboxedFs([F.dataDir], grants, probeDirsFor(F.runtimeDir));
    var fsOnly = makeSandboxedFs([F.dataDir], grants, []);
    // Confined to the runtime dir only, to pin the bound the socket answer respects.
    var confined = makeSandboxedFs([F.dataDir], grants, [F.runtimeDir]);

    t("facade surface", keysOf(netTrusted), [
        "accessSync", "constants", "createWriteStream", "existsSync", "mkdirSync",
        "promises", "readFileSync", "realpathSync", "rmSync", "statSync",
        "unlinkSync", "writeFileSync",
    ].sort());

    if (F.onUnix) {
        t("endpoint inside a disclosed dir is visible", confined.existsSync(F.insideSock), true);
        t("socket outside every disclosed dir is not", confined.existsSync(F.outsideSock), false);
        t("no endpoint trust, no socket at all", fsOnly.existsSync(F.insideSock), false);
        // A bridged endpoint: a symlink in the disclosed dir to a socket elsewhere. The
        // resolved target is outside ipcDirs; containment on the lexical path admits it.
        t("bridged endpoint (symlink to a socket elsewhere) is found",
            confined.existsSync(F.bridgedSock), true);
        // But a symlink to a non-socket outside stays hidden: the type test is on target.
        t("a symlink to a non-socket outside stays hidden",
            confined.existsSync(F.bridgedToFile), false);
    } else {
        out.push("SKIP  socket cases (win32 endpoints are named pipes, not files)");
    }

    t("outside file stays hidden", netTrusted.existsSync(F.outsideFile), false);
    t("missing path answers false", netTrusted.existsSync(F.dataDir + "/nope"), false);
    // Answers false like the real one, and says why on stderr - the line below this
    // block is the announcement, and its absence is the Windows named-pipe silence.
    t("a rejected path shape answers false", netTrusted.existsSync(42), false);
    t("dataDir file still visible", fsOnly.existsSync(F.dataDir + "/inside.txt"), true);
    t("realpath of a disclosed dir", netTrusted.realpathSync(TMP), TMP);
    t("realpath inside dataDir", fsOnly.realpathSync(F.dataDir), F.dataDir);
    threw("disclosed dir denied without trust", function() { fsOnly.realpathSync(TMP); }, "realpath denied");
    threw("undisclosed dir denied", function() { netTrusted.realpathSync(F.dataDir + "/.."); }, "realpath denied");
    threw("reading outside still denied", function() { netTrusted.readFileSync(F.outsideFile); }, "read denied");

    // A facade that lies about its shape sends callers down a fallback they did not want.
    t("realpathSync honours its options argument",
        Buffer.isBuffer(netTrusted.realpathSync(F.dataDir, "buffer")), true);
    t("realpathSync.native is present", typeof netTrusted.realpathSync.native, "function");
    t("native answers inside the sandbox", netTrusted.realpathSync.native(F.dataDir), F.dataDir);
    threw("native obeys the same gate", function() { fsOnly.realpathSync.native(TMP); }, "realpath denied");

    // ── The real entry points, not only their helpers ────────────────────
    // Everything else in this file calls internal helpers directly. A plugin arrives through
    // proxiedRequire and through the hardened ambient globals, and neither was ever driven
    // here: a whole gate could have been deleted and nothing would have noticed.
    var reqNone = makeRequireProxy(new Set(), fsOnly, F.dataDir, "Probe/mod.native.ts", mockedProcess);
    t("a safe module is handed over", typeof reqNone("path").join, "function");
    t("the plugin data dir is virtual and frozen",
        Object.isFrozen(reqNone("@luna/native-data")), true);
    t("and it names the plugin's own dir", reqNone("@luna/native-data").dir, F.dataDir);
    threw("a blocked module without trust demands trust",
        function() { reqNone("net"); }, "TRUST_REQUIRED:net");
    threw("fs without trust demands it too",
        function() { reqNone("fs"); }, "TRUST_REQUIRED:fs");
    threw("a relative id is refused as a path",
        function() { reqNone("./evil"); }, "paths not allowed");
    threw("an absolute id is refused as a path",
        function() { reqNone("/etc/passwd"); }, "paths not allowed");
    // A forbidden module is NOT refused at require time: it hands back an inert stub, and
    // the throw is owed by the member call. Both halves are the contract.
    threw("calling into a forbidden stub throws",
        function() { reqNone("child_process").spawn("ls"); },
        "child_process.spawn is not available");
    var reqNet = makeRequireProxy(new Set(["net"]), fsOnly, F.dataDir, "Probe/mod.native.ts", mockedProcess);
    t("the same module WITH trust is handed over",
        typeof reqNet("net").createServer, "function");
    var reqFs = makeRequireProxy(new Set(["fs"]), fsOnly, F.dataDir, "Probe/mod.native.ts", mockedProcess);
    t("fs trust yields the sandboxed facade, never the real module",
        reqFs("fs") === fsOnly, true);

    // The eval and Function gates at their real boundary; only the scanner was exercised.
    // Calling eval here is the assertion, not a slip: this is the sandbox's own hardened
    // eval, and the test is that it REFUSES the dynamic import rather than running it.
    threw("eval refuses a dynamic import",
        function() { eval("import('node:fs')"); }, "eval blocked");
    threw("the Function constructor refuses one too",
        function() { Function("return import('node:fs')"); }, "Function blocked");

    // The hardened ambient globals a plugin actually sees.
    threw("process.binding is blocked",
        function() { globalThis.process.binding("fs"); }, "is not allowed");
    t("Bun.spawn is neutered", typeof globalThis.Bun.spawn, "undefined");
    t("Bun.write is neutered", typeof globalThis.Bun.write, "undefined");
    threw("the ambient fetch refuses and points at the trusted path",
        function() { globalThis.fetch("http://x"); }, "fetch is not available");

    // ── Mutable globals: no gate may consult one at decision time ────────
    var body = [
        'module.exports.pollute = function(mode, arg) {',
        '  if (mode === "indexOf") Array.prototype.indexOf = function() { return 0; };',
        '  if (mode === "startsWith") String.prototype.startsWith = function(s) { return s !== "file:"; };',
        '  if (mode === "objectProto") Object.prototype.XDG_RUNTIME_DIR = "/tmp/forged";',
        '  if (mode === "objectCreate") {',
        '    var real = Object.create;',
        '    Object.create = function(p, d) { var o = real(p, d); if (p === null) arg.grabbed = o; return o; };',
        '    return real;',
        '  }',
        '  if (mode === "envKey") globalThis.Bun.env.HOME = "/evil/home";',
        '  if (mode === "tmpdir") { globalThis.Bun.env.TMPDIR = arg; globalThis.Bun.env.TMP = arg; globalThis.Bun.env.TEMP = arg; }',
        '  if (mode === "isSocket") Object.getPrototypeOf(arg).isSocket = function() { return true; };',
        '  if (mode === "promiseReject") { var r = Promise.reject; Promise.reject = function() { return { forged: true }; }; return r; }',
        '  if (mode === "objectToJSON") {',
        '    Object.prototype.toJSON = function() { return { forged: true }; };',
        '    Array.prototype.toJSON = function() { arg.saw = this[0] && this[0][1]; return { forgedArr: true }; };',
        '    return "planted";',
        '  }',
        '  if (mode === "functionApply") {',
        '    var r = Function.prototype.apply;',
        '    Function.prototype.apply = function() { arg.hit = true; return "POISONED"; };',
        '    return r;',
        '  }',
        '  if (mode === "abHasInstance") {',
        '    try { Object.defineProperty(ArrayBuffer, Symbol.hasInstance, { value: function() { return false; } }); return "defined"; }',
        '    catch (e) { return "refused"; }',
        '  }',
        '  if (mode === "stringBinding") {',
        '    try { globalThis.String = function Evil() { return "hijacked"; }; } catch (_) {}',
        '    return String("ok");',
        '  }',
        '  if (mode === "setBinding") {',
        '    try { globalThis.Set = function Evil() { this.has = function() { return true; }; }; } catch (_) {}',
        '    return (new Set()).constructor.name;',
        '  }',
        '  if (mode === "moduleId") { String.prototype.startsWith = function() { return true; }; String.prototype.slice = function() { return "buffer"; }; }',
        '  if (mode === "arrayFrom") { var r = Array.from; Array.from = function() { return [""]; }; return r; }',
        '  if (mode === "arraySome") { var r = Array.prototype.some; Array.prototype.some = function() { return false; }; return r; }',
        '  if (mode === "callProp") {',
        '    String.prototype.startsWith.call = function() { return true; };',
        '    String.prototype.slice.call = function() { return "buffer"; };',
        '    Array.prototype.some.call = function() { return false; };',
        '    Set.prototype.has.call = function() { return true; };',
        '  }',
        '  if (mode === "unCallProp") {',
        '    delete String.prototype.startsWith.call; delete String.prototype.slice.call;',
        '    delete Array.prototype.some.call; delete Set.prototype.has.call;',
        '  }',
        '};',
    ].join("\\n");
    var plug = evalPlugin(body, function() { throw new Error("no require"); },
        function() { throw new Error("no fetch"); }, mockedProcess);

    plug.exports.pollute("objectProto");
    t("a planted env key does not surface", plainEnv.XDG_RUNTIME_DIR, undefined);
    t("nor on a freshly built env",
        endpointEnvFor(new Set(["fs"]), F.runtimeDir).env.XDG_RUNTIME_DIR, undefined);
    delete Object.prototype.XDG_RUNTIME_DIR;

    var savedIndexOf = Array.prototype.indexOf;
    plug.exports.pollute("indexOf");
    threw("realpath gate survives a polluted indexOf",
        function() { netTrusted.realpathSync("/etc"); }, "realpath denied");
    Array.prototype.indexOf = savedIndexOf;

    var savedStartsWith = String.prototype.startsWith;
    plug.exports.pollute("startsWith");
    var leaked;
    try { netTrusted.readFileSync(F.outsideFile); leaked = true; }
    catch (_) { leaked = false; }
    String.prototype.startsWith = savedStartsWith;
    t("containment survives a polluted startsWith", leaked, false);

    // Object is never frozen and restorePrototypes only covers prototypes; the env
    // builder must not reach the live statics: a hook would be handed the next
    // plugin's env object and could read a key that plugin alone was granted.
    // Each case carries a control: the corruption is proved live first, otherwise a
    // green assertion would only mean the attack never armed.
    var sink = {};
    var realCreate = plug.exports.pollute("objectCreate", sink);
    var freshEnv = endpointEnvFor(new Set(["net"]), F.runtimeDir).env;
    var grabbedWhileBuilding = sink.grabbed;
    var control = Object.create(null);
    Object.create = realCreate;
    t("control: the Object.create hook was live", sink.grabbed === control, true);
    t("a hooked Object.create is not handed the env", grabbedWhileBuilding, undefined);
    t("and the env was still built", freshEnv.XDG_RUNTIME_DIR, F.runtimeDir);

    // process.env is globalThis.Bun.env, writable by any plugin: the allowed keys
    // are snapshotted at load rather than re-read per registration.
    var realHome = mockedProcess.env.HOME;
    plug.exports.pollute("envKey");
    t("control: the env write landed", globalThis.Bun.env.HOME, "/evil/home");
    t("a poisoned env key does not reach a later plugin",
        makeMockedProcess({ K: "1" }).env.HOME, realHome);
    globalThis.Bun.env.HOME = realHome;

    // Same object, reached through os.tmpdir(), which re-reads it on every call. os.tmpdir
    // reads TMPDIR on POSIX and TEMP/TMP on Windows; poison all three and restore the
    // originals - a single-var poison would not arm the control on the other platform.
    var savedTmpEnv = { TMPDIR: globalThis.Bun.env.TMPDIR, TMP: globalThis.Bun.env.TMP, TEMP: globalThis.Bun.env.TEMP };
    plug.exports.pollute("tmpdir", F.scratch);
    t("control: os.tmpdir() now answers the planted dir",
        require("os").tmpdir(), F.scratch);
    // Membership again, and this doubles as the assertion that the planted dir did NOT
    // enter the list: only the captured hostTmpDir (whose canonical form is TMP) is there.
    var afterPoison = probeDirsFor(null);
    t("the probe dirs keep the captured temp dir", _ArrayPrototypeIndexOf(afterPoison, TMP) !== -1, true);
    t("the planted temp dir never enters the list",
        _ArrayPrototypeIndexOf(afterPoison, F.scratch), -1);
    // Assigning undefined would write the string "undefined"; delete an absent original.
    ["TMPDIR", "TMP", "TEMP"].forEach(function(k) {
        if (savedTmpEnv[k] === undefined) delete globalThis.Bun.env[k];
        else globalThis.Bun.env[k] = savedTmpEnv[k];
    });

    // The socket test must read the mode, not a method shared by every stat object.
    var statsProto = Object.getPrototypeOf(realFs.statSync(F.dataDir));
    var savedIsSocket = statsProto.isSocket;
    plug.exports.pollute("isSocket", realFs.statSync(F.dataDir));
    t("control: every stat now claims to be a socket",
        realFs.statSync(F.runtimePlain).isSocket(), true);
    t("a plain file stays hidden despite a patched isSocket",
        confined.existsSync(F.runtimePlain), false);
    statsProto.isSocket = savedIsSocket;

    // A swapped Promise.reject must not turn a denial into a resolved value.
    var realReject = plug.exports.pollute("promiseReject");
    var denied = fsOnly.promises.realpath(TMP);
    t("control: the Promise.reject swap was live",
        Promise.reject(new Error("control")).forged, true);
    // Every async denial, not just the one that owns the captured reject: X_OK builds its
    // own rejection, and a swap reaches that site too.
    var deniedExec = fsOnly.promises.access(F.dataDir, realFs.constants.X_OK);
    Promise.reject = realReject;
    t("a denied promises realpath still rejects", denied.forged, undefined);
    t("a denied promises access still rejects", deniedExec.forged, undefined);
    // Settled shapes only: a forged denial is a plain object; calling catch on it would
    // throw and lose the report that names which site failed.
    settle(denied);
    settle(deniedExec);

    // Every arm owes a REJECTION, not a synchronous throw. A caller that chains .catch()
    // instead of wrapping in try/catch sees nothing at all when a gate throws at the call
    // site. The shape of the denial is part of the contract, not a detail.
    var promiseArms = [
        ["realpath", function() { return fsOnly.promises.realpath(TMP); }],
        ["readFile", function() { return fsOnly.promises.readFile(F.outsideFile); }],
        ["writeFile", function() { return fsOnly.promises.writeFile(F.outsideFile, "x"); }],
        ["mkdir", function() { return fsOnly.promises.mkdir(F.outsideFile); }],
        ["stat", function() { return fsOnly.promises.stat(F.outsideFile); }],
        ["unlink", function() { return fsOnly.promises.unlink(F.outsideFile); }],
        ["rm", function() { return fsOnly.promises.rm(F.outsideFile); }],
        ["access", function() { return fsOnly.promises.access(F.outsideFile); }],
    ];
    for (var ai = 0; ai < promiseArms.length; ai++) {
        var outcome;
        try {
            var arm = promiseArms[ai][1]();
            // Thenable-ness is NOT the property under test: a resolved promise carries
            // .catch too; a defeated containment gate would delete the file outside the
            // sandbox, resolve, and read as PASS; measured. Bun.peek reads the settled
            // state synchronously, which lets the probe assert the rejection itself without
            // becoming async, and staying synchronous is what keeps a hang unreachable.
            outcome = (arm && typeof arm.then === "function")
                ? Bun.peek.status(arm)
                : "not-a-promise";
            settle(arm);
        } catch (_) { outcome = "threw"; }
        t("promises " + promiseArms[ai][0] + " rejects instead of throwing", outcome, "rejected");
    }

    // The web tier the body encoder reads members off, on a value one plugin supplied while
    // another plugin's request is being built. Request/Headers/Response were already frozen;
    // these three arrived with the fetch path and were the tier left writable.
    t("FormData prototype is frozen",
        typeof FormData === "undefined" || Object.isFrozen(FormData.prototype), true);
    t("Blob prototype is frozen",
        typeof Blob === "undefined" || Object.isFrozen(Blob.prototype), true);
    t("URLSearchParams prototype is frozen",
        typeof URLSearchParams === "undefined" || Object.isFrozen(URLSearchParams.prototype), true);
    // The constructor, not just Promise.reject: ipcFetch's executor receives resolve/reject
    // and stores them in a map shared by every plugin. A swapped Promise would hand one
    // plugin the settlement functions of another's request.
    t("the promise constructor is captured, not read live",
        typeof _RealPromise !== "undefined" && _RealPromise === realPromiseCtor, true);

    // Freezing a primordial protects the OBJECT; the NAME is a separate property. The host
    // builds its trust store and its fs grant stores with the bare global: a rebound
    // Set hands it a store that answers the membership question with the plugin's answer.
    var seenByPlugin = plug.exports.pollute("setBinding");
    // Restore before asserting: where the rebind succeeds it stays in place for every later
    // case, and a probe that poisons its own successors reports a crash instead of a verdict.
    try { globalThis.Set = realSetCtor; } catch (_) {}
    t("a plugin cannot rebind the Set global", seenByPlugin, "Set");
    t("the Set global is pinned, not just frozen",
        Object.getOwnPropertyDescriptor(globalThis, "Set").writable, false);

    // Freezing a prototype and rebinding a global are separate powers. The six prototypes
    // left unfrozen for npm compat still get their NAME pinned, or a rebound String rewrites
    // the URL of another plugin's fetch, credentials and cookie jar with it.
    var seenString = plug.exports.pollute("stringBinding");
    try { globalThis.String = realStringCtor; } catch (_) {}
    t("a plugin cannot rebind the String global", seenString, "ok");
    t("the names whose prototypes stay unfrozen are pinned anyway",
        ["Object", "Array", "String", "Promise", "Error"].every(function(n) {
            var d = Object.getOwnPropertyDescriptor(globalThis, n);
            return d && d.writable === false;
        }), true);
    // And pinning the name must not have cost what the npm-compat note protects.
    String.prototype.__probeMarker = 1;
    t("pinning the name still allows prototype assignment",
        String.prototype.__probeMarker, 1);
    delete String.prototype.__probeMarker;

    // The backing buffer, not just its views, and Buffer with it: freezing the buffer
    // module's exports is shallow; every body encode was reading writable members.
    t("ArrayBuffer prototype is frozen", Object.isFrozen(ArrayBuffer.prototype), true);
    t("Buffer prototype is frozen", Object.isFrozen(Buffer.prototype), true);
    t("Buffer statics are frozen", Object.isFrozen(Buffer), true);
    // A hasInstance trap receives the object under test: it reads another plugin's body
    // while answering true. ArrayBuffer carried no own one, which is why it was definable.
    t("a hasInstance trap cannot be planted on ArrayBuffer",
        plug.exports.pollute("abHasInstance"), "refused");

    // The Function wrapper must not read apply off an unfrozen prototype: a poisoned one is
    // handed the real, unwrapped constructor after the dynamic-import scan already ran.
    var applyProbe = {};
    var realApply = plug.exports.pollute("functionApply", applyProbe);
    var built = Function("return 1 + 1");
    Function.prototype.apply = realApply;
    t("control: the apply swap was live", typeof realApply, "function");
    t("the Function wrapper does not read apply live", applyProbe.hit, undefined);
    t("and it still builds a real function", typeof built, "function");

    // The options guard returns what it authorized. A Proxy whose has trap lies while
    // ownKeys tells the truth is legal, and Node finds fd by enumeration; refusing on
    // 'fd' in opts never saw it: fd 1 is stdout, which is the IPC pipe to Rust.
    var smuggled = new Proxy({ fd: 1, encoding: "utf8" }, {
        has: function(t2, k) { return k === "fd" || k === "fs" ? false : k in t2; },
    });
    t("control: the smuggling proxy hides fd from the in operator", "fd" in smuggled, false);
    // Absence has to read as a failure, not a ReferenceError: a probe that throws loses the
    // report that names which site gave way.
    var haveOptsGuard = typeof authorizedWriteStreamOpts === "function";
    t("the options guard returns what it authorized", haveOptsGuard, true);
    var sanitized = haveOptsGuard ? authorizedWriteStreamOpts(smuggled) : { fd: 1 };
    t("the write-stream guard never carries a smuggled fd", sanitized.fd, undefined);
    t("and it keeps a legitimate option", sanitized.encoding, "utf8");
    // The bare string arrives saturated like any other shape, NOT passed through: handing the
    // engine a string makes IT build the options object, and read every name off its prototype.
    var bare = haveOptsGuard ? authorizedWriteStreamOpts("utf8") : null;
    t("a bare encoding string is carried as an own option", bare && bare.encoding, "utf8");

    // The class the saturation exists for, and the one BOTH harnesses are blind to: an
    // engine-read name we leave unset resolves up Object.prototype. The sweep cannot reach it:
    // it poisons members that already EXIST on the six tracked prototypes, and none of these
    // names does.
    var mustShadow = ["fd", "fs", "construct", "write", "writev", "objectMode", "signal",
        "start", "flags", "mode", "autoClose", "final", "destroy", "path"];
    function unshadowed(opts) {
        var saturated = haveOptsGuard ? authorizedWriteStreamOpts(opts) : undefined;
        // A shape handed back unchanged IS the defect this replaces; it fails on its own
        // line: a throw would surface as "the probe itself threw" and lose which shape gave way.
        if (saturated === null || saturated === undefined || typeof saturated !== "object")
            return "handed back unsaturated";
        var missing = [];
        for (var mi = 0; mi < mustShadow.length; mi++) {
            if (_ObjectGetOwnPropertyDescriptor(saturated, mustShadow[mi]) === undefined)
                missing.push(mustShadow[mi]);
        }
        return missing.join(",");
    }
    // Per shape, not per name: the defect was a shape that skipped the guard entirely.
    t("an options object leaves no engine-read name to the prototype",
        unshadowed({ encoding: "utf8" }), "");
    t("a no-options call is saturated too", unshadowed(undefined), "");
    t("and so is a bare encoding string", unshadowed("utf8"), "");

    // JSON.stringify calls an INHERITED toJSON, and restorePrototypes preserves additions by
    // design: a planted one replaces the payload of every line this process writes, which is
    // a forged command to Rust, not a corrupted one.
    var jsonProbe = {};
    t("control: the toJSON plant took",
        plug.exports.pollute("objectToJSON", jsonProbe), "planted");
    t("control: a plain literal is replaced by it",
        JSON.parse(_JSONStringify({ id: 7, ok: true })).forged, true);
    var haveIpcLine = typeof ipcLine === "function";
    t("the IPC line builder exists", haveIpcLine, true);
    // Depth matters as much as the top level: the array trap is handed the array itself, so
    // it both reads the slot it gets and replaces it, and headers carry a credential.
    var secretLine = { id: 7, headers: [["authorization", "Bearer PLUGIN-B-SECRET"]] };
    var built = JSON.parse(haveIpcLine ? ipcLine(secretLine) : _JSONStringify(secretLine));
    t("a planted toJSON cannot replace an IPC line", built.id, 7);
    t("a nested array survives as an array", built.headers && built.headers[0][0], "authorization");
    t("and the array trap never saw the credential", jsonProbe.saw, undefined);
    delete Object.prototype.toJSON;
    delete Array.prototype.toJSON;

    // The abort tier. Freezing covers the shared prototype; the signal itself comes from the
    // plugin. The instance half needs the captured invoker.
    t("EventTarget prototype is frozen",
        typeof EventTarget === "undefined" || Object.isFrozen(EventTarget.prototype), true);
    t("AbortSignal prototype is frozen",
        typeof AbortSignal === "undefined" || Object.isFrozen(AbortSignal.prototype), true);
    t("the listener invoker is captured, not read off the signal",
        typeof _AddEventListener === "function", true);
    var forged = { addEventListener: function(_evt, cb) { forged.stolen = cb; } };
    try { _AddEventListener(forged, "abort", function() {}); } catch (_) { /* expected */ }
    t("a forged signal never receives the abort closure", forged.stolen, undefined);

    // The require gate keys on canonicalize(id); reading id.startsWith/id.slice live
    // would let a plugin fold every id onto a safe name and be handed the real module.
    var savedStarts2 = String.prototype.startsWith;
    var savedSlice = String.prototype.slice;
    plug.exports.pollute("moduleId");
    t("control: id.slice is corrupted", "child_process".slice(0), "buffer");
    t("canonicalize resists a corrupted slice", canonicalize("node:fs"), "fs");
    t("a blocked module is not reclassified safe", isSafe("fs"), false);
    String.prototype.startsWith = savedStarts2;
    String.prototype.slice = savedSlice;

    // The directory-grant check must not read a corruptible Array.from.
    var realFrom = plug.exports.pollute("arrayFrom");
    var grantOne = { readFiles: new Set(), writeFiles: new Set(), dirs: new Set([F.dataDir]) };
    var stillDenied;
    try { stillDenied = !isReadable(F.outsideFile, [F.dataDir], grantOne); }
    catch (_) { stillDenied = true; }
    t("control: Array.from is corrupted", Array.from(new Set(["x"]))[0], "");
    Array.from = realFrom;
    t("grant containment survives a corrupted Array.from", stillDenied, true);

    // The dynamic-import gate must not read a corruptible Array.prototype.some.
    var realSome = plug.exports.pollute("arraySome");
    var stillBlocks = containsDynamicImport("const x = import('node:child_process');");
    t("control: Array.prototype.some is corrupted", [1].some(function() { return true; }), false);
    Array.prototype.some = realSome;
    t("the import gate survives a corrupted some", stillBlocks, true);

    // The .call OWN-property vector: the _m.call(...) idiom would read this settable
    // property on the extensible method object, which restorePrototypes never removes.
    // The bound invokers read the intrinsic [[Call]] instead; every gate must hold.
    plug.exports.pollute("callProp");
    t("control: startsWith.call is poisoned",
        String.prototype.startsWith.call("zzz", "node:"), true);
    t("canonicalize resists a poisoned .call", canonicalize("node:child_process"), "child_process");
    t("isSafe resists a poisoned .call", isSafe("child_process"), false);
    t("the import gate resists a poisoned .call",
        containsDynamicImport("const x = import('node:child_process');"), true);
    var callDenied;
    try { callDenied = !isReadable(F.outsideFile, [F.dataDir], grantOne); }
    catch (_) { callDenied = true; }
    t("fs containment resists a poisoned .call", callDenied, true);
    plug.exports.pollute("unCallProp");

    } catch (e) {
        // A throw used to be INVISIBLE: it escaped into the net.listen callback the probe
        // runs under, which does not fail the process on its own, and then
        // rl.on("close", exit(0)), registered by native-host.cjs for the real runtime,
        // fired on the immediate EOF of the null stdin Command::output() hands the child,
        // winning the race against Bun's nonzero exit. Measured: a crashed probe reported
        // success. Naming it as a failure is what makes a crash count as one.
        failed = true;
        out.push("FAIL  the probe itself threw  " + ((e && e.stack) || e));
    }

    try { realFs.rmSync(F.scratch, { recursive: true, force: true }); } catch (_) {}
    // Drain before leaving: a write to a PIPED stderr is cut at the pipe's 65536-byte
    // capacity if the process exits first, measured deterministically.
    hostStderr.write(out.join("\\n") + "\\n", function () { hostExit(failed ? 1 : 0); });
})();
`;

function run() {
    const fn = new Function(
        "require", "module", "exports", "__dirname", "__filename", "probeFixtures",
        src + "\n" + ASSERTIONS,
    );
    fn(require, { exports: {} }, {}, path.dirname(HOST), HOST, {
        scratch, dataDir, outsideFile, runtimeDir, runtimePlain,
        insideSock, outsideSock, bridgedSock, bridgedToFile,
        aliasReal, aliasLink, onUnix,
    });
}

// A symlink under the disclosed dir pointing at a regular file outside: its target is
// not a socket; discovery must reject it even though its path is contained.
const bridgedToFile = path.join(runtimeDir, "bridged-file");

if (onUnix) {
    // Endpoints: one directly in the disclosed dir, one outside it, and one reached by
    // a symlink in the disclosed dir whose socket lives outside (the bridged case).
    const inside = net.createServer();
    inside.listen(insideSock, () => {
        const outside = net.createServer();
        outside.listen(outsideSock, () => {
            fs.symlinkSync(outsideSock, bridgedSock);
            fs.symlinkSync(outsideFile, bridgedToFile);
            run();
        });
    });
} else {
    run();
}
