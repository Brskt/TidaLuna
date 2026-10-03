// The slot redux-saga's PUT runner calls once the Rust filter has rewritten it (`tap_saga_put`,
// module_capture.rs). Pinned here, before any chunk and any plugin runs: a script that took the
// slot first would see, and could cancel, every action a saga emits. The host installs the one
// handler behind it once its registry exists; until then a put goes straight through.
(function () {
    var handler = null;
    var seen = false;
    var pin = function (name, value) {
        Object.defineProperty(globalThis, name, {
            value: value,
            writable: false,
            configurable: false,
            enumerable: false
        });
    };
    pin('__lunaSagaPut', function (action, dispatch) {
        seen = true;
        return handler === null ? dispatch(action) : handler(action, dispatch);
    });
    // One-shot: the host takes it at init, and any later caller is refused.
    pin('__lunaSagaPutInstall', function (h) {
        if (handler !== null || typeof h !== 'function') return false;
        handler = h;
        return true;
    });
    // Whether a put ever reached the slot. TIDAL's root saga puts from its first tick, long
    // before the host finds the store, so none by then means the rewrite did not match.
    pin('__lunaSagaPutSeen', function () {
        return seen;
    });
})();
