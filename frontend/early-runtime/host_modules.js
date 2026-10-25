// Sink for TIDAL's real module namespaces: the Rust capture filter appends
// __LUNA_CAP(id, ns) to React-family chunks, letting plugins resolve the host React.
window.__lunaHostModules = window.__lunaHostModules || {};
(function () {
    // Captured here because this script runs before any chunk and before any plugin: a plugin
    // can leave enumerable names on Object.prototype, and `for..in` over the namespace or `in`
    // against the sink would then read them as the object's own.
    var ownNames = Object.keys;
    var ownDesc = Object.getOwnPropertyDescriptor;
    var define = Object.defineProperty;
    // Defined, not assigned; an assignment would run a setter left on Object.prototype
    // for that name instead of storing the value.
    function put(into, name, value) {
        define(into, name, { value: value, writable: true, enumerable: true, configurable: true });
    }
    // Each id owns an object filled by copy: the Rust filter hands one namespace to every id of a
    // chunk, and sharing it would let one capture reach the others. First writer wins a name, missing
    // ones merge in; an id can span several chunks (react-dom was split), and replacing drops exports.
    globalThis.__LUNA_CAP = function (id, ns) {
        var held = ownDesc(window.__lunaHostModules, id);
        var into = held === undefined ? {} : held.value;
        if (held === undefined) put(window.__lunaHostModules, id, into);
        var names = ownNames(ns);
        for (var i = 0; i < names.length; i++) {
            if (ownDesc(into, names[i]) === undefined) put(into, names[i], ns[names[i]]);
        }
    };
})();
