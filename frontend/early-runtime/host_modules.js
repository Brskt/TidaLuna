// Sink for TIDAL's real module namespaces: the Rust capture filter appends
// __LUNA_CAP(id, ns) to React-family chunks, letting plugins resolve the host React.
window.__lunaHostModules = window.__lunaHostModules || {};
(function () {
    // Captured here because this script runs before any chunk and before any plugin: a plugin
    // can leave enumerable names on Object.prototype, and `for..in` over the namespace or `in`
    // against the sink would then read them as the object's own.
    var ownNames = Object.keys;
    var ownDesc = Object.getOwnPropertyDescriptor;
    // First writer wins each NAME it already holds: the real react chunk is modulepreloaded and
    // runs before any lazy vendor chunk; a later same-id capture cannot clobber a valid one.
    // The names it does not hold are taken from the later chunk instead of dropped, because one
    // id can legitimately name several chunks: react-dom's exports have already been split
    // once, and replacing rather than unioning leaves a namespace that satisfies its validator
    // with an export missing, which surfaces as a bare TypeError inside a plugin.
    globalThis.__LUNA_CAP = function (id, ns) {
        var existing = window.__lunaHostModules[id];
        if (existing === undefined) {
            window.__lunaHostModules[id] = ns;
            return;
        }
        var names = ownNames(ns);
        for (var i = 0; i < names.length; i++) {
            if (ownDesc(existing, names[i]) === undefined) existing[names[i]] = ns[names[i]];
        }
    };
})();
