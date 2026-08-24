// Appended into the SAME `new Function(...)` body as native-host.cjs, exactly like
// tests/unit/native_runtime/host_probe.cjs does it: every host-private function
// (assertRead, isSafe, canonicalize, makeSandboxedFs, evalPlugin, respond, ...) is
// reachable by bare name, not through an export surface the host doesn't have.
//
// Job: enumerate every own member of six prototypes + every global binding, poison
// each ONE AT A TIME, run a fixed oracle suite, record which oracles broke, restore,
// move to the next. Uses `await` only on already-settled/microtask work (never a
// timer or real I/O), so nothing here yields far enough for the readline "close"
// handler to preempt it before hostExit() runs at the end.
;(async function adversary() {
    var F = probeFixtures;
    // Stdin is already at EOF in a non-interactive shell, so readline's own "close" ->
    // hostExit(0) handler (installed near the bottom of the host source) would otherwise
    // race one of the `await`s below and kill this process long before the sweep. And,
    // unlike host_probe.cjs, this harness can't stay purely synchronous: ipcFetch's own
    // `await encodeBody(...)` means observing its outbound write needs a real microtask
    // tick. This harness owns exit; the host's own readline wiring should not.
    if (typeof rl !== "undefined" && rl && typeof rl.removeAllListeners === "function") {
        rl.removeAllListeners("close");
    }

    // ---- Harness-private pristine captures (must survive whatever we poison) -----
    var H_ObjectGetOwnPropertyNames = Object.getOwnPropertyNames;
    var H_ObjectGetOwnPropertySymbols = Object.getOwnPropertySymbols;
    var H_ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    var H_ObjectDefineProperty = Object.defineProperty;
    var H_ObjectKeys = Object.keys;
    var H_JSONStringify = JSON.stringify;
    var H_JSONParse = JSON.parse;
    var H_RealProxy = Proxy;

    // ---- Oracle suite: cheap, in-process, run after EVERY poison ------------------
    // Deliberately blind to the fix's internal names (ipcLine/inertValue/isTrusted/
    // authorizedWriteStreamOpts/_RealPromise never appear below) so the same suite is
    // meaningful against a host that predates all of them. Where a test needs a Set,
    // it does `new Set()` (the LIVE global) on purpose: that is what real host/plugin
    // code does too, and it is exactly what lets a poisoned Set rebind show up here.
    var ORACLES = [];
    function oracle(name, fn) { ORACLES[ORACLES.length] = { name: name, fn: fn }; }

    // A gate either denies (throws, any message) or wrongly grants (returns/doesn't
    // throw). Throwing for the WRONG reason still fails closed, so it counts as safe.
    function mustDeny(fn) {
        try { fn(); return { pass: false, detail: "did not throw - access granted" }; }
        catch (e) { return { pass: true, detail: String(e && e.message)}; }
    }

    oracle("assertRead denies an outside file", function() {
        return mustDeny(function() {
            assertRead(F.outsideFile, [F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() });
        });
    });
    oracle("assertWrite denies an outside file", function() {
        return mustDeny(function() {
            assertWrite(F.outsideFile, [F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() });
        });
    });
    oracle("assertDelete denies an outside file", function() {
        return mustDeny(function() { assertDelete(F.outsideFile, [F.dataDir]); });
    });
    oracle("assertMkdir denies an outside file", function() {
        return mustDeny(function() {
            assertMkdir(F.outsideFile, [F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() });
        });
    });
    oracle("isSafe('fs') stays false", function() {
        var v = isSafe("fs"); return { pass: v === false, detail: String(v) };
    });
    oracle("isBlocked('fs') stays true", function() {
        var v = isBlocked("fs"); return { pass: v === true, detail: String(v) };
    });
    oracle("isForbidden('child_process') stays true", function() {
        var v = isForbidden("child_process"); return { pass: v === true, detail: String(v) };
    });
    oracle("canonicalize('node:fs') === 'fs'", function() {
        var v = canonicalize("node:fs"); return { pass: v === "fs", detail: String(v) };
    });
    oracle("reachesLocalEndpoint(net-only) === true", function() {
        // Built by .add(), never `new Set(["net"])`: the array form consumes
        // Symbol.iterator, so poisoning that iterator emptied the ORACLE'S OWN set and the
        // gate then answered false quite correctly. That is a broken test setup, not a
        // broken gate; the host never builds a trust set from a literal either, it
        // uses an indexed .add() loop. The Set here stays the LIVE global on purpose, so a
        // rebound Set still shows up.
        var trusted = new Set();
        trusted.add("net");
        var v = reachesLocalEndpoint(trusted); return { pass: v === true, detail: String(v) };
    });
    oracle("reachesLocalEndpoint(no-trust) === false", function() {
        var v = reachesLocalEndpoint(new Set()); return { pass: v === false, detail: String(v) };
    });
    oracle("containsDynamicImport still blocks import()", function() {
        var v = containsDynamicImport("const x = import('fs');");
        return { pass: v === true, detail: String(v) };
    });
    oracle("isReadable denies an outside file with no grant", function() {
        var real;
        try { real = canonicalizeFsPath(F.outsideFile); } catch (e) { return { pass: true, detail: "canonicalize threw: " + e.message }; }
        try {
            var v = isReadable(real, [F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() });
            return { pass: v === false, detail: String(v) };
        } catch (e) { return { pass: true, detail: "threw (fail-closed): " + String(e && e.message) }; }
    });
    oracle("require('fs') denied without trust", function() {
        var sfs = makeSandboxedFs([F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() }, []);
        var trustedModules = new Set(); // deliberately live: reacts to a poisoned Set rebind
        var req = makeRequireProxy(trustedModules, sfs, F.dataDir, "adversary-plugin", { env: {} });
        return mustDeny(function() { req("fs"); });
    });
    oracle("require('worker_threads') denied without trust", function() {
        var trustedModules = new Set();
        var req = makeRequireProxy(trustedModules, null, F.dataDir, "adversary-plugin", { env: {} });
        return mustDeny(function() { req("worker_threads"); });
    });
    oracle("Function() constructor still computes correctly", function() {
        try {
            var built = Function("return 40+2");
            var v = built();
            return { pass: v === 42, detail: String(v) };
        } catch (e) { return { pass: false, detail: "threw: " + String(e && e.message) }; }
    });
    oracle("evalPlugin still executes plugin body", function() {
        try {
            var m = evalPlugin("module.exports.v = 1+1;",
                function() { throw new Error("no require"); },
                function() { throw new Error("no fetch"); }, { env: {} });
            return { pass: m.exports.v === 2, detail: String(m.exports.v) };
        } catch (e) { return { pass: false, detail: "threw: " + String(e && e.message) }; }
    });

    function capturedWrite(fn) {
        var captured = null;
        var realWrite = hostStdout.write;
        hostStdout.write = function(chunk) { if (captured === null) captured = chunk; return true; };
        try { fn(); } finally { hostStdout.write = realWrite; }
        return captured;
    }
    function parseIpcLine(captured, checks) {
        if (typeof captured !== "string") return { pass: false, detail: "wrote nothing" };
        var parsed;
        try { parsed = H_JSONParse(captured); } catch (e) { return { pass: false, detail: "invalid JSON: " + captured }; }
        return { pass: checks(parsed), detail: captured.trim() };
    }
    oracle("respond() IPC line round-trips id/fields (toJSON-proof)", function() {
        var c = capturedWrite(function() { respond("adv-id-1", { ok: true, custom: "value" }); });
        return parseIpcLine(c, function(p) { return p && p.id === "adv-id-1" && p.ok === true && p.custom === "value"; });
    });
    oracle("respondError() IPC line round-trips id/error (toJSON-proof)", function() {
        var c = capturedWrite(function() { respondError("adv-id-2", "boom"); });
        return parseIpcLine(c, function(p) { return p && p.id === "adv-id-2" && p.error === "boom"; });
    });
    oracle("sendCancel() IPC line round-trips reqId (toJSON-proof)", function() {
        var c = capturedWrite(function() { sendCancel(9999); });
        return parseIpcLine(c, function(p) { return p && p.type === "net.fetch.cancel" && p.reqId === 9999; });
    });
    oracle("ipcFetch outbound line round-trips url/reqId (toJSON-proof)", async function() {
        // ipcFetch is `async function` and does `await encodeBody(...)` BEFORE the
        // hostStdout.write() that emits the net.fetch line, so the write lands on a
        // later microtask tick, after a synchronous capture window would already have
        // restored the real write. The awaits below exist for that, not a synchronous wrapper.
        var idBefore = nextFetchId;
        var writes = [];
        var realWrite = hostStdout.write;
        hostStdout.write = function(chunk) { writes[writes.length] = chunk; return true; };
        var outer;
        try { outer = makeIpcFetch("adversary-fetch-probe")("http://adversary.invalid/probe-path", {}); }
        catch (e) { hostStdout.write = realWrite; return { pass: false, detail: "threw synchronously: " + String(e && e.message) }; }
        await null; await null; await null; await null;
        hostStdout.write = realWrite;
        if (outer && typeof outer.catch === "function") outer.catch(function() {});
        try { settle(idBefore); } catch (_) {}
        var c = writes.length ? writes[0] : null;
        return parseIpcLine(c, function(p) {
            return p && p.type === "net.fetch" && p.url === "http://adversary.invalid/probe-path" && typeof p.reqId === "number";
        });
    });
    oracle("fs.promises rejects (never throws) for denied ops", function() {
        var fsOnly = makeSandboxedFs([F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() }, []);
        var arms = [
            ["realpath", function() { return fsOnly.promises.realpath(F.outsideFile); }],
            ["readFile", function() { return fsOnly.promises.readFile(F.outsideFile); }],
            ["writeFile", function() { return fsOnly.promises.writeFile(F.outsideFile, "x"); }],
            ["mkdir", function() { return fsOnly.promises.mkdir(F.outsideFile); }],
            ["stat", function() { return fsOnly.promises.stat(F.outsideFile); }],
            ["unlink", function() { return fsOnly.promises.unlink(F.outsideFile); }],
            ["rm", function() { return fsOnly.promises.rm(F.outsideFile); }],
            ["access", function() { return fsOnly.promises.access(F.outsideFile); }],
        ];
        var bad = [];
        for (var i = 0; i < arms.length; i++) {
            // Thenable-ness is not the property: a RESOLVED promise carries .catch too, so
            // a defeated containment gate would do the real filesystem work, resolve, and
            // read as a pass. Bun.peek reads the settled state synchronously.
            var state = "threw";
            try {
                var r = arms[i][1]();
                state = (r && typeof r.then === "function") ? Bun.peek.status(r) : "not-a-promise";
                if (r && typeof r.catch === "function") r.catch(function() {});
            } catch (e) { state = "threw"; }
            if (state !== "rejected") bad[bad.length] = arms[i][0] + "=" + state;
        }
        return { pass: bad.length === 0, detail: bad.length ? ("did not reject: " + bad.join(",")) : "all reject cleanly" };
    });
    oracle("createWriteStream never leaks a smuggled fd", function() {
        // realFs itself is frozen by the host's own freezeModuleExports() (present in
        // both versions), so monkeypatching realFs.createWriteStream to capture options
        // silently no-ops: this must exercise the real call instead. fd:0 (our own
        // stdin, already detached from the readline interface above), never fd:1 (the
        // IPC/report channel this harness still needs). Node sets .fd synchronously
        // from an explicit numeric fd option, so no write/read is needed to observe it.
        var facade = makeSandboxedFs([F.dataDir], { readFiles: new Set(), writeFiles: new Set(), dirs: new Set() }, []);
        var smuggled = new H_RealProxy({ fd: 0, encoding: "utf8" }, {
            has: function(t, k) { return (k === "fd" || k === "fs") ? false : (k in t); },
        });
        var stream = null;
        var threw = null;
        try { stream = facade.createWriteStream(F.dataDir + "/adversary-probe.txt", smuggled); }
        catch (e) { threw = String(e && e.message); }
        var leaked = !!(stream && stream.fd === 0);
        if (stream) {
            try { stream.on("error", function() {}); } catch (_) {}
            try { stream.destroy(); } catch (_) {}
        }
        return { pass: !leaked, detail: threw ? ("threw: " + threw) : ("stream.fd=" + (stream && stream.fd)) };
    });

    oracle("ipcFetch never hands its abort closure to a forged signal", async function() {
        // A forged `signal` (any object with its own addEventListener) is a plugin-
        // supplied fetch(url, {signal}) argument, not something that needs poisoning:
        // it exercises whether the host calls signal.addEventListener(...) directly
        // (handing the closure to the forger's own method) or through a captured,
        // uncurried EventTarget.prototype.addEventListener invoker that throws on a
        // non-EventTarget receiver instead.
        var forged = { stolen: null, addEventListener: function(evt, cb) { forged.stolen = cb; } };
        var idBefore = nextFetchId;
        var realWrite = hostStdout.write;
        hostStdout.write = function() { return true; };
        var outer;
        try { outer = makeIpcFetch("adversary-signal-probe")("http://adversary.invalid/signal-probe", { signal: forged }); }
        catch (e) { hostStdout.write = realWrite; return { pass: false, detail: "threw synchronously: " + String(e && e.message) }; }
        await null; await null; await null; await null;
        hostStdout.write = realWrite;
        if (outer && typeof outer.catch === "function") outer.catch(function() {});
        try { settle(idBefore); } catch (_) {}
        return { pass: forged.stolen === null, detail: forged.stolen === null ? "never called" : "forged.addEventListener received the real onAbort closure" };
    });

    // The sweep above can only arm a name that ALREADY EXISTS, and a pristine Array.prototype
    // owns no numeric index: an accessor planted at one was never reachable from it, and
    // no oracle ever drove an array-consuming gate with plugin-shaped content either. That
    // pair of gaps hid a real hole: a hole in a headers array reads straight through to
    // Array.prototype, and the forged pair reached the outbound net.fetch line.
    //
    // The arming is deliberately scoped INSIDE this oracle and never added to the general
    // enumeration. Measured: a global index poison silently destroys the sweep's own
    // bookkeeping, because `results[results.length] = x` redirects to the inherited setter
    // instead of growing the array, so every verdict for that target collapses to an empty
    // result set and the failure scan reports 0 without ever running.
    // For the same reason the capture below is a single variable, and the check is a manual
    // loop rather than `.some()`, which an outer target may have poisoned.
    oracle("ipcFetch does not read a forged pair out of a sparse headers hole", async function() {
        var desc0 = H_ObjectGetOwnPropertyDescriptor(Array.prototype, "0");
        var desc1 = H_ObjectGetOwnPropertyDescriptor(Array.prototype, "1");
        // A setter is not optional: with a getter alone, every `arr[arr.length] = x` in the
        // process throws instead of appending, and the host swallows that one frame up.
        H_ObjectDefineProperty(Array.prototype, "0", {
            configurable: true,
            get: function() { return ["x-forged-0", "evil0"]; },
            set: function() {},
        });
        H_ObjectDefineProperty(Array.prototype, "1", {
            configurable: true,
            get: function() { return ["x-forged-1", "evil1"]; },
            set: function() {},
        });
        function restoreIndices() {
            if (desc0 === undefined) delete Array.prototype["0"];
            else H_ObjectDefineProperty(Array.prototype, "0", desc0);
            if (desc1 === undefined) delete Array.prototype["1"];
            else H_ObjectDefineProperty(Array.prototype, "1", desc1);
        }
        // Holes at 0 and 1, a real pair at 2: two holes prove the guard is not a
        // special case for a leading hole.
        var sparseHeaders = [];
        sparseHeaders[2] = ["authorization", "Bearer real-secret"];
        var idBefore = nextFetchId;
        var captured = null;
        var realWrite = hostStdout.write;
        hostStdout.write = function(chunk) { if (captured === null) captured = chunk; return true; };
        var outer;
        try {
            outer = makeIpcFetch("adversary-sparse-headers-probe")(
                "http://adversary.invalid/sparse-headers", { headers: sparseHeaders });
        } catch (e) {
            hostStdout.write = realWrite;
            restoreIndices();
            return { pass: false, detail: "threw synchronously: " + String(e && e.message) };
        }
        await null; await null; await null; await null;
        hostStdout.write = realWrite;
        restoreIndices();
        if (outer && typeof outer.catch === "function") outer.catch(function() {});
        try { settle(idBefore); } catch (_) {}
        return parseIpcLine(captured, function(p) {
            if (!p || p.type !== "net.fetch" || !p.headers || typeof p.headers.length !== "number") {
                return false;
            }
            var hasForged = false, hasReal = false;
            for (var hi = 0; hi < p.headers.length; hi++) {
                var pair = p.headers[hi];
                if (!pair) continue;
                if (pair[0] === "x-forged-0" || pair[0] === "x-forged-1") hasForged = true;
                if (pair[0] === "authorization" && pair[1] === "Bearer real-secret") hasReal = true;
            }
            return !hasForged && hasReal;
        });
    });

    async function runOracleSuite() {
        var results = [];
        for (var i = 0; i < ORACLES.length; i++) {
            var r;
            // A throw is a FAILURE, never "inconclusive". Scoring it as a pass was measured
            // to hide three separate typo-class regressions in the host (a misspelled
            // constant in `canonicalize`, a misspelled Set in `isSafe`, an unguarded index
            // in `makeSandboxedFs`), each of which left this sweep at exit 0 with 0
            // failures while mislabeling 225, 225 and 675 oracle results.
            //
            // The defence for the old behaviour was that a poison legitimately breaks an
            // oracle's own scaffolding. Measured against a correct host across every armed
            // target: that happens ZERO times, so the trade bought nothing. It can happen
            // for a future oracle whose fixture uses a poisonable intrinsic, and then this
            // goes red on a healthy host, which is the right way round. A false red is
            // investigated; a false green is not.
            try { r = await ORACLES[i].fn(); }
            catch (e) { r = { pass: false, detail: "oracle '" + ORACLES[i].name + "' threw: " + String(e && e.message) }; }
            results[results.length] = { name: ORACLES[i].name, pass: r.pass, detail: r.detail };
        }
        return results;
    }

    // ---- Poison + restore machinery ----------------------------------------------
    function snapshot(owner, key) { return H_ObjectGetOwnPropertyDescriptor(owner, key); }
    function restore(owner, key, desc) {
        try {
            if (desc === undefined) delete owner[key];
            else H_ObjectDefineProperty(owner, key, desc);
        } catch (_) { /* best effort restore; frozen/non-configurable owners never armed anyway */ }
    }

    var ADVERSARIAL_VALUE = {
        has: true, includes: true, some: true, every: false, indexOf: 0, lastIndexOf: 0,
        startsWith: true, endsWith: true, slice: "buffer", substring: "buffer", substr: "buffer",
        valueOf: 0, join: "", toString: "[object Poisoned]",
    };

    function poisonValueFor(key, sentinel) {
        if (key === Symbol.iterator) {
            // Mirrors the real defect: a spread/`Array.from` over this sees an
            // immediately-exhausted iterator, the same shape a poisoned
            // Array.prototype[Symbol.iterator] gives `await member(...(args||[]))`.
            return function() {
                sentinel.called = true;
                return { next: function() { return { done: true, value: undefined }; } };
            };
        }
        if (typeof key === "string" && H_ObjectKeys(ADVERSARIAL_VALUE).indexOf(key) !== -1) {
            var fixed = ADVERSARIAL_VALUE[key];
            return function() { sentinel.called = true; return fixed; };
        }
        return function() { sentinel.called = true; return true; };
    }

    var report = { host: F.hostPath, totalPoisons: 0, armedCount: 0, targets: [], failures: [] };

    async function testOneTarget(label, applyPoison, restoreFn) {
        report.totalPoisons++;
        var sentinel = { called: false };
        var armed = false;
        var poisonError = null;
        try { armed = !!applyPoison(sentinel); } catch (e) { poisonError = String(e && e.message); }
        var oracleResults = armed ? await runOracleSuite() : null;
        try { restoreFn(); } catch (_) {}
        if (armed) report.armedCount++;
        var failed = [];
        if (oracleResults) {
            for (var i = 0; i < oracleResults.length; i++) {
                if (oracleResults[i].pass === false) failed[failed.length] = oracleResults[i];
            }
        }
        if (failed.length) report.failures[report.failures.length] = { label: label, failed: failed };
        // Keep the full per-target log lightweight: only armed targets and any
        // failures are worth writing out at 130+150 targets.
        if (armed || poisonError) {
            report.targets[report.targets.length] = {
                label: label, armed: armed, poisonError: poisonError, failedCount: failed.length,
            };
        }
    }

    // ---- Enumerate: six named prototypes, own string + symbol keys ---------------
    var PROTOS = [
        ["Object.prototype", Object.prototype],
        ["Array.prototype", Array.prototype],
        ["Function.prototype", Function.prototype],
        ["String.prototype", String.prototype],
        ["Promise.prototype", Promise.prototype],
        ["Error.prototype", Error.prototype],
    ];
    var protoTargetCount = 0;
    for (var pi = 0; pi < PROTOS.length; pi++) {
        var protoName = PROTOS[pi][0];
        var proto = PROTOS[pi][1];
        var names = H_ObjectGetOwnPropertyNames(proto);
        var symbols = H_ObjectGetOwnPropertySymbols(proto);
        var keys = names.concat(symbols);
        for (var ki = 0; ki < keys.length; ki++) {
            protoTargetCount++;
            await (async function(protoName, proto, key) {
                var desc = snapshot(proto, key);
                var label = protoName + "." + String(key);
                await testOneTarget(label, function(sentinel) {
                    var poison = poisonValueFor(key, sentinel);
                    try {
                        proto[key] = poison;
                        return proto[key] === poison;
                    } catch (_) { return false; }
                }, function() { restore(proto, key, desc); });
            })(protoName, proto, keys[ki]);
        }
    }

    // ---- toJSON additions (do not pre-exist on either prototype) ------------------
    await (async function() {
        var descO = snapshot(Object.prototype, "toJSON");
        await testOneTarget("Object.prototype.toJSON (planted)", function(sentinel) {
            try {
                Object.prototype.toJSON = function() { sentinel.called = true; return { forged: true }; };
                return Object.prototype.toJSON !== undefined;
            } catch (_) { return false; }
        }, function() { restore(Object.prototype, "toJSON", descO); });
    })();
    await (async function() {
        var descA = snapshot(Array.prototype, "toJSON");
        await testOneTarget("Array.prototype.toJSON (planted)", function(sentinel) {
            try {
                Array.prototype.toJSON = function() { sentinel.called = true; return { forgedArr: true }; };
                return Array.prototype.toJSON !== undefined;
            } catch (_) { return false; }
        }, function() { restore(Array.prototype, "toJSON", descA); });
    })();

    // ---- Global bindings: every own name on globalThis ----------------------------
    var globalNames = H_ObjectGetOwnPropertyNames(globalThis);
    var globalTargetCount = 0;
    for (var gi = 0; gi < globalNames.length; gi++) {
        var name0 = globalNames[gi];
        // "globalThis" is a genuine, self-referential OWN property (globalThis.globalThis
        // === globalThis), not special syntax. Poisoning it rebinds what the bare
        // identifier `globalThis` resolves to for every LATER statement in this same
        // script, including this harness's own restore step, permanently corrupting
        // everything poisoned afterward. A real subprocess-per-poison design wouldn't
        // have this problem; this in-process one just skips the one target that eats
        // itself. (Verified: without this guard, every global-binding target enumerated
        // after "globalThis" comes back with an impossible "already gone" descriptor.)
        if (name0 === "globalThis") { continue; }
        globalTargetCount++;
        await (async function(name) {
            var desc = snapshot(globalThis, name);
            // require/module/exports/__dirname/__filename are pinned via a getter that
            // THROWS by design ("[sandbox] globalThis.X is not available"). Reading the
            // "original" value to hand to the evil Proxy's construct trap must not blow
            // up the sweep over a property we were never going to arm anyway.
            var original;
            try { original = globalThis[name]; } catch (_) { original = undefined; }
            var label = "globalThis." + name;
            await testOneTarget(label, function(sentinel) {
                var evil = new H_RealProxy(function adversaryEvil() { sentinel.called = true; return true; }, {
                    get: function(t, p) {
                        sentinel.called = true;
                        if (p === "prototype") return (original && original.prototype) ? original.prototype : {};
                        return function() { return true; };
                    },
                    apply: function() { sentinel.called = true; return true; },
                    construct: function() {
                        sentinel.called = true;
                        return { has: function() { return true; }, get: function() { return true; },
                            then: function(res) { if (res) res(true); } };
                    },
                });
                try {
                    globalThis[name] = evil;
                    return globalThis[name] === evil;
                } catch (_) { return false; }
            }, function() { restore(globalThis, name, desc); });
        })(globalNames[gi]);
    }

    // ---- Bespoke: the real "call" IPC handler, args spread vs Reflect.apply -------
    // Drives the ACTUAL rl.on("line", ...) handler via a synthetic line event (not a
    // reimplementation of its logic), while Array.prototype[Symbol.iterator] is
    // poisoned to yield nothing, the exact shape `await member(...(args||[]))` sees
    // when a plugin has already corrupted the iterator (and restorePrototypes, which
    // only tracks string-named members, never undoes it for the NEXT call either).
    async function bespokeCallHandlerIteratorPoison() {
        if (typeof rl === "undefined" || !rl || typeof rl.emit !== "function") {
            return { label: "Array.prototype[Symbol.iterator] poisoned during the real \"call\" IPC handler",
                armed: false, poisonError: "no rl.emit available in this host", failedCount: 0 };
        }
        var writes = [];
        var realWrite = hostStdout.write;
        hostStdout.write = function(chunk) { writes[writes.length] = chunk; return true; };
        var regId = "adv-reg-" + Date.now();
        var callId = "adv-call-" + Date.now();
        try {
            rl.emit("line", JSON.stringify({
                type: "register", id: regId, name: "adversary-echo-plugin",
                code: "module.exports.echo = function() { var out = []; for (var i = 0; i < arguments.length; i++) out[i] = arguments[i]; return out; };",
                trust: {}, dataDir: null,
            }));
            await null; await null; await null;
            var origIter = Array.prototype[Symbol.iterator];
            Array.prototype[Symbol.iterator] = function() {
                return { next: function() { return { done: true, value: undefined }; } };
            };
            try {
                rl.emit("line", JSON.stringify({
                    type: "call", id: callId, name: "adversary-echo-plugin", fn: "echo", args: [11, 22, 33],
                }));
                await null; await null; await null; await null;
            } finally {
                Array.prototype[Symbol.iterator] = origIter;
            }
        } finally {
            hostStdout.write = realWrite;
        }
        var callResponse = null;
        for (var i = 0; i < writes.length; i++) {
            try {
                var p = JSON.parse(writes[i]);
                if (p && p.id === callId) callResponse = p;
            } catch (_) {}
        }
        var gotArgs = callResponse && callResponse.result;
        var argsSurvived = Array.isArray(gotArgs) && gotArgs.length === 3
            && gotArgs[0] === 11 && gotArgs[1] === 22 && gotArgs[2] === 33;
        var failed = argsSurvived ? [] : [{
            name: "the \"call\" handler must not silently drop args when Symbol.iterator is corrupted",
            pass: false, detail: "echo returned " + H_JSONStringify(gotArgs) + " for args [11,22,33] (raw: " + H_JSONStringify(callResponse) + ")",
        }];
        return {
            label: "Array.prototype[Symbol.iterator] poisoned during the real \"call\" IPC handler",
            armed: true, poisonError: null, failedCount: failed.length, failedOracles: failed,
        };
    }

    // ---- Bespoke: a poison at an ATTACKER-CHOSEN identity, not an existing member --
    // The sweep above enumerates each prototype's own members and corrupts them one at a
    // time, so it can only ever arm a name that already exists. The host's own containers
    // are keyed by the plugin NAME, which arrives on the wire and exists on no prototype
    // until a plugin puts it there, outside the enumeration by construction. Measured:
    // eight structurally different container shapes all leaked a victim's real call
    // arguments this way, and every one of them was invisible to a syntax-level check,
    // because what makes a container safe is its prototype at write time, not its spelling.
    // So this asks the outcome instead: with a fresh accessor planted at the exact name a
    // real register/call carries, does the round-trip still answer correctly?
    // A NEVER-REUSED name per combination is load-bearing. A write that is NOT intercepted
    // leaves a real own property on the container, which then shadows the prototype for
    // every later combination testing the same name and hides the hits.
    async function bespokeIdentityPoison() {
        var label = "a fresh accessor at the plugin's own name, during the real register/call";
        if (typeof rl === "undefined" || !rl || typeof rl.emit !== "function") {
            return { label: label, armed: false, poisonError: "no rl.emit available in this host", failedCount: 0 };
        }
        var protos = [
            ["Object.prototype", Object.prototype],
            ["Array.prototype", Array.prototype],
            ["Function.prototype", Function.prototype],
        ];
        var failed = [];
        var stamp = Date.now();
        for (var pi = 0; pi < protos.length; pi++) {
            var protoLabel = protos[pi][0];
            var proto = protos[pi][1];
            var pluginName = "adv-identity-" + pi + "-" + stamp;
            var regId = "adv-idreg-" + pi + "-" + stamp;
            var callId = "adv-idcall-" + pi + "-" + stamp;
            var fired = false;
            var writes = [];
            var realWrite = hostStdout.write;
            try {
                Object.defineProperty(proto, pluginName, {
                    configurable: true,
                    get: function() { fired = true; return undefined; },
                    set: function() { fired = true; },
                });
            } catch (e) {
                failed[failed.length] = { name: "the harness must be able to plant at " + protoLabel,
                    pass: false, detail: String(e && e.message) };
                continue;
            }
            hostStdout.write = function(chunk) { writes[writes.length] = chunk; return true; };
            try {
                rl.emit("line", JSON.stringify({
                    type: "register", id: regId, name: pluginName,
                    code: "module.exports.probe = function(x) { return { echoed: x }; };",
                    trust: {}, dataDir: null,
                }));
                await null; await null; await null;
                rl.emit("line", JSON.stringify({
                    type: "call", id: callId, name: pluginName, fn: "probe", args: ["secret-payload"],
                }));
                await null; await null; await null; await null;
            } finally {
                hostStdout.write = realWrite;
                try { delete proto[pluginName]; } catch (_) {}
            }
            var reg = null;
            var call = null;
            for (var wi = 0; wi < writes.length; wi++) {
                try {
                    var parsed = JSON.parse(writes[wi]);
                    if (parsed && parsed.id === regId) reg = parsed;
                    if (parsed && parsed.id === callId) call = parsed;
                } catch (_) {}
            }
            var registered = !!(reg && reg.ok === true);
            var answered = !!(call && call.ok === true && call.result && call.result.echoed === "secret-payload");
            if (!registered || !answered || fired) {
                failed[failed.length] = {
                    name: "a plugin named after a poisoned " + protoLabel + " key must still register and answer",
                    pass: false,
                    detail: "registered=" + registered + " answered=" + answered + " accessorFired=" + fired
                        + " reg=" + H_JSONStringify(reg) + " call=" + H_JSONStringify(call),
                };
            }
        }
        return { label: label, armed: true, poisonError: null, failedCount: failed.length, failedOracles: failed };
    }

    // ---- Bespoke: the executor-capture attack (Promise), only visible if we ------
    // ---- actually drive ipcFetch's internal `new Promise` while it's poisoned. ----
    await (async function bespokePromiseExecutorCapture() {
        var originalPromise = globalThis.Promise;
        function EvilPromise(executor) {
            var fakeResolve = function() {}; fakeResolve.__isEvil = true;
            var fakeReject = function() {}; fakeReject.__isEvil = true;
            try { executor(fakeResolve, fakeReject); } catch (_) {}
        }
        var idBefore = nextFetchId;
        var threw = null;
        try {
            globalThis.Promise = EvilPromise;
            makeIpcFetch("bespoke-promise-probe")("http://adversary.invalid/promise-probe", {});
            // Same reason as the ipcFetch IPC-line oracle above: ipcFetch is `async
            // function` and does `await encodeBody(...)` BEFORE reaching `new Promise(...)`
            // (or `new _RealPromise` post-fix), so restoring globalThis.Promise right after
            // the synchronous call above would undo the poison before it was ever read.
            await null; await null; await null; await null;
        } catch (e) { threw = String(e && e.message); }
        finally { globalThis.Promise = originalPromise; }
        var entry = pendingFetches.get(idBefore);
        var captured = !!(entry && entry.resolve && entry.resolve.__isEvil === true);
        if (entry) { try { settle(idBefore); } catch (_) {} }
        report.totalPoisons++;
        report.armedCount++;
        var label = "globalThis.Promise (executor resolve/reject capture via ipcFetch)";
        var failed = captured
            ? [{ name: "ipcFetch's resolve/reject must come from the real Promise", pass: false,
                 detail: "attacker's executor received the resolve/reject pair stored in pendingFetches" }]
            : [];
        report.targets[report.targets.length] = { label: label, armed: true, poisonError: threw, failedCount: failed.length };
        if (failed.length) report.failures[report.failures.length] = { label: label, failed: failed };
    })();

    var callHandlerResult = await bespokeCallHandlerIteratorPoison();
    report.totalPoisons++;
    if (callHandlerResult.armed) report.armedCount++;
    report.targets[report.targets.length] = {
        label: callHandlerResult.label, armed: callHandlerResult.armed,
        poisonError: callHandlerResult.poisonError, failedCount: callHandlerResult.failedCount,
    };
    if (callHandlerResult.failedCount) {
        report.failures[report.failures.length] = { label: callHandlerResult.label, failed: callHandlerResult.failedOracles };
    }

    var identityResult = await bespokeIdentityPoison();
    report.totalPoisons++;
    if (identityResult.armed) report.armedCount++;
    report.targets[report.targets.length] = {
        label: identityResult.label, armed: identityResult.armed,
        poisonError: identityResult.poisonError, failedCount: identityResult.failedCount,
    };
    if (identityResult.failedCount) {
        report.failures[report.failures.length] = { label: identityResult.label, failed: identityResult.failedOracles };
    }

    report.protoTargetCount = protoTargetCount;
    report.globalTargetCount = globalTargetCount;
    return report;
})().then(function(report) {
    hostStderr.write("\n===ADVERSARY-REPORT-START===\n");
    hostStderr.write(JSON.stringify(report) + "\n");
    hostStderr.write("===ADVERSARY-REPORT-END===\n");
    for (var fi = 0; fi < report.failures.length; fi++) {
        var f = report.failures[fi];
        for (var fj = 0; fj < f.failed.length; fj++) {
            hostStderr.write("FAIL  " + f.label + "  ->  " + f.failed[fj].name
                + "  (" + f.failed[fj].detail + ")\n");
        }
    }
    hostStderr.write("armed " + report.armedCount + " of " + report.totalPoisons
        + " poisons; " + report.failures.length + " target(s) broke a gate\n");
    try { realFs.rmSync(probeFixtures.scratch, { recursive: true, force: true }); } catch (_) {}
    // Leaving with 0 while reporting failures in JSON is exactly what a driver reads as
    // success: the disease this harness exists to catch. The exit code carries the
    // verdict. Drain stderr first: a piped write is cut at the pipe's 65536-byte capacity
    // if the process leaves before it flushes, measured.
    var code = report.failures.length ? 1 : 0;
    hostStderr.write("", function() { hostExit(code); });
}, function(err) {
    hostStderr.write("ADVERSARY HARNESS CRASHED: " + String(err && err.stack || err) + "\n",
        function() { hostExit(1); });
});
