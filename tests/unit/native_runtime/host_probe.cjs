// Policy probe for frontend/scripts/native-host.cjs, driven by host_script.rs.
//
// The script under test hardens its own realm and has no exports, so it is loaded
// exactly the way it loads a plugin - through new Function - and the assertions are
// appended into that same scope. Anything short of this tests a copy, not the file
// the binary embeds.
//
// No data is interpolated into the generated body: fixtures arrive as an extra
// parameter, so the only concatenated strings are the host source and this file's
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

// A stand-in for the session runtime dir, so the value under test is ours and the
// assertions mean the same thing on a machine that has no such dir at all.
const runtimeDir = path.join(scratch, "run");
fs.mkdirSync(runtimeDir);
// A plain file inside a disclosed dir: what a corrupted socket test would expose.
const runtimePlain = path.join(runtimeDir, "plain.txt");
fs.writeFileSync(runtimePlain, "x");

// Named pipes are not filesystem sockets, so the socket cases only exist on unix.
const onUnix = process.platform !== "win32";
const insideSock = path.join(runtimeDir, "endpoint.sock");
const outsideSock = path.join(scratch, "stray.sock");
// A bridged endpoint: a symlink under the disclosed dir whose target is a socket
// elsewhere (the SSH/Flatpak case). Created below once outsideSock exists.
const bridgedSock = path.join(runtimeDir, "bridged.sock");

// A disclosed dir reached through a symlinked prefix (macOS /var -> /private/var): the
// lexical name and its realpath differ, and a caller may hand existsSync either one.
// Symlinks need privilege on Windows, so this fixture is unix-only.
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
    function t(name, got, want) {
        var ok = JSON.stringify(got) === JSON.stringify(want);
        if (!ok) failed = true;
        out.push((ok ? "PASS  " : "FAIL  ") + name + "  got=" + JSON.stringify(got));
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

    var F = probeFixtures;
    var grants = { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() };
    var TMP = realFs.realpathSync(require("os").tmpdir());

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
    // realpath under /private/var) probeDirsFor returns both forms, so the canonical TMP
    // is present alongside its lexical alias rather than being the sole entry.
    t("probe dirs keep the temp dir with no runtime dir",
        _ArrayPrototypeIndexOf(probeDirsFor(null), TMP) !== -1, true);

    // A dir reached through a symlinked prefix is stored under BOTH names, so existsSync
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

    // Object is never frozen and restorePrototypes only covers prototypes, so the env
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

    // process.env is globalThis.Bun.env, writable by any plugin, so the allowed keys
    // are snapshotted at load rather than re-read per registration.
    var realHome = mockedProcess.env.HOME;
    plug.exports.pollute("envKey");
    t("control: the env write landed", globalThis.Bun.env.HOME, "/evil/home");
    t("a poisoned env key does not reach a later plugin",
        makeMockedProcess({ K: "1" }).env.HOME, realHome);
    globalThis.Bun.env.HOME = realHome;

    // Same object, reached through os.tmpdir(), which re-reads it on every call. os.tmpdir
    // reads TMPDIR on POSIX and TEMP/TMP on Windows, so poison all three and restore the
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
    Promise.reject = realReject;
    t("a denied promises realpath still rejects", denied.forged, undefined);
    denied.catch(function() {}); // the rejection is the assertion; do not leave it unhandled

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
    // The bound invokers read the intrinsic [[Call]] instead, so every gate must hold.
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

    hostStderr.write(out.join("\\n") + "\\n");
    try { realFs.rmSync(F.scratch, { recursive: true, force: true }); } catch (_) {}
    hostExit(failed ? 1 : 0);
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
// not a socket, so discovery must reject it even though its path is contained.
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
