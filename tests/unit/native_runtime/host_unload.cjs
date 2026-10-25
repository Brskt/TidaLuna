// Unload probe for frontend/scripts/native-host.cjs, driven like host_probe.cjs: the host is loaded
// through new Function with the assertions appended to its scope, where they drive the real
// rl.on("line") handler. A module is observed from outside: it bumps a counter on globalThis.

const fs = require("fs");
const path = require("path");
const os = require("os");

const HOST = process.argv[2];
if (!HOST) {
    process.stderr.write("usage: bun host_unload.cjs <path to native-host.cjs>\n");
    process.exit(2);
}
const src = fs.readFileSync(HOST, "utf8");

const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "luna-host-unload-")));
const nativeRoot = path.join(scratch, "native");
fs.mkdirSync(nativeRoot);
const dataDir = path.join(nativeRoot, "Unload");

const ASSERTIONS = `
;(async function probe() {
    var out = [];
    var failed = false;
    try { rl.removeAllListeners("close"); } catch (_) {}
    function t(name, got, want) {
        var ok = got === want;
        if (!ok) failed = true;
        out.push((ok ? "PASS  " : "FAIL  ") + name + "  got=" + String(got));
    }
    var F = probeFixtures;
    var writes = [];
    hostStdout.write = function(chunk) { writes[writes.length] = chunk; return true; };
    var realSetTimeout = setTimeout;
    function pause(ms) { return new Promise(function(r) { realSetTimeout(r, ms); }); }
    var seq = 0;
    function send(cmd) { cmd.id = "u" + (++seq); rl.emit("line", JSON.stringify(cmd)); return cmd.id; }
    async function ask(cmd) {
        var id = send(cmd);
        await pause(20);
        for (var i = 0; i < writes.length; i++) {
            var p; try { p = JSON.parse(writes[i]); } catch (_) { continue; }
            if (p && p.id === id) return p;
        }
        return null;
    }
    function lines(type) {
        var res = [];
        for (var i = 0; i < writes.length; i++) {
            try { var p = JSON.parse(writes[i]); if (p && p.type === type) res[res.length] = p; } catch (_) {}
        }
        return res;
    }
    function register(name, code, trust) {
        return ask({ type: "register", name: name, code: code, trust: trust || {},
            dataDir: F.dataDir, nativeRoot: F.nativeRoot });
    }
    var g = globalThis;
    function n(key) { return g[key] || 0; }

    try {
        var BODY = [
            "var timers = require('timers');",
            "var P = '__KEY__';",
            "function tick(k) { return function() { globalThis[P + k] = (globalThis[P + k] || 0) + 1; }; }",
            "setInterval(tick('free'), 5);",
            "timers.setInterval(tick('mod'), 5);",
            "setTimeout(tick('late'), 1500);",
            "setImmediate(function again() { tick('imm')(); setImmediate(again); });",
            "exports.startFetch = function() {",
            "  fetch('http://127.0.0.1:9/x').then(tick('fetchok'), function(e) { globalThis[P + 'fetcherr'] = e.message; });",
            "};",
            "exports.fetchNow = function() { return fetch('http://127.0.0.1:9/y'); };",
            "exports.ping = function() { return 'pong'; };",
            "exports.sleepPromisified = function() { return require('util').promisify(setTimeout)(10, 'v'); };",
            "exports.sleepViaPromises = function() { return require('timers/promises').setTimeout(10, 'w'); };",
        ].join("\\n");
        function body(key) { return BODY.split("__KEY__").join(key); }

        // Two modules side by side: the unload of one must not reach the other.
        var r1 = await register("Unload/a.native.ts", body("a_"), { http: true, https: true });
        var r2 = await register("Unload/b.native.ts", body("b_"), { http: true, https: true });
        t("both modules register", r1 && r1.ok === true && r2 && r2.ok === true, true);
        var sp = await ask({ type: "call", name: "Unload/b.native.ts", fn: "sleepPromisified", args: [] });
        await pause(40);
        sp = await ask({ type: "call", name: "Unload/b.native.ts", fn: "sleepPromisified", args: [] });
        t("util.promisify(setTimeout) still resolves with the value", sp && sp.result, "v");
        var sw = await ask({ type: "call", name: "Unload/b.native.ts", fn: "sleepViaPromises", args: [] });
        t("timers/promises.setTimeout still resolves with the value", sw && sw.result, "w");
        await ask({ type: "call", name: "Unload/a.native.ts", fn: "startFetch", args: [] });
        await pause(60);
        var freeBefore = n("a_free"), modBefore = n("a_mod"), immBefore = n("a_imm");
        t("the module's intervals run before the unload", freeBefore > 3 && modBefore > 3 && immBefore > 3, true);
        var bBefore = n("b_free");

        t("the module holds a grant store while loaded", grantStores.has("Unload/a.native.ts"), true);
        var c = await ask({ type: "cleanup", name: "Unload/a.native.ts" });
        t("cleanup is answered", c && c.ok === true, true);
        t("its grant store is gone", grantStores.has("Unload/a.native.ts"), false);
        t("its scope is gone", moduleScopes["Unload/a.native.ts"], undefined);
        var freeAt = n("a_free"), modAt = n("a_mod"), immAt = n("a_imm");
        await pause(250);
        t("its setInterval stopped", n("a_free"), freeAt);
        t("its timers.setInterval stopped", n("a_mod"), modAt);
        t("its setImmediate chain stopped", n("a_imm"), immAt);
        t("its pending setTimeout never fired", n("a_late"), 0);
        t("its pending fetch was rejected as unloaded",
            /unloaded/.test(String(g.a_fetcherr)), true);
        t("Rust was told to drop that fetch", lines("net.fetch.cancel").length, 1);
        t("the other module kept running", n("b_free") > bBefore, true);
        var gone = await ask({ type: "call", name: "Unload/a.native.ts", fn: "ping", args: [] });
        t("a call after the unload finds no module", gone && /not registered/.test(String(gone.error)), true);
        var still = await ask({ type: "call", name: "Unload/b.native.ts", fn: "ping", args: [] });
        t("the other module still answers", still && still.result, "pong");

        // A second evaluation of the same name replaces the first: what the first started goes.
        var rr = await register("Unload/b.native.ts", body("b2_"), { http: true, https: true });
        t("re-register answers", rr && rr.ok === true, true);
        var b1At = n("b_free");
        await pause(120);
        t("the replaced evaluation's interval stopped", n("b_free"), b1At);
        t("the replacing evaluation runs", n("b2_free") > 3, true);

        // A load that throws after starting something leaves nothing behind.
        var thrown = await register("Unload/c.native.ts",
            "setInterval(function() { globalThis.c_free = (globalThis.c_free || 0) + 1; }, 5); throw new Error('boom');");
        t("a throwing load is refused", thrown && typeof thrown.error === "string", true);
        var cAt = n("c_free");
        await pause(80);
        t("what a failed load started was stopped", n("c_free"), cAt);

        var unknown = await ask({ type: "cleanup", name: "Unload/never.native.ts" });
        t("cleanup of an unknown module is harmless", unknown && unknown.ok === true, true);

        await ask({ type: "cleanup", name: "Unload/b.native.ts" });
    } catch (e) {
        failed = true;
        out.push("FAIL  the probe itself threw  " + ((e && e.stack) || e));
    }
    try { realFs.rmSync(F.scratch, { recursive: true, force: true }); } catch (_) {}
    hostStderr.write(out.join("\\n") + "\\n", function () { hostExit(failed ? 1 : 0); });
})();
`;

const fn = new Function(
    "require", "module", "exports", "__dirname", "__filename", "probeFixtures",
    src + "\n" + ASSERTIONS,
);
fn(require, { exports: {} }, {}, path.dirname(HOST), HOST, { scratch, dataDir, nativeRoot });
