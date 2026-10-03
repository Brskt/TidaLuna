// The slot TIDAL's saved-volume restore reads through once `route_volume_restore`
// (module_capture.rs) has rewritten it. TIDAL restores a level per account; this install plays
// one. Once anyone chose a level (`chosen`), TIDAL restores nothing and the page mirrors Rust's
// level (`resyncOutputLevel`, frontend/src/volume.ts); before that, TIDAL's saved level is
// restored and seeds Rust's. Pinned before any chunk or plugin runs, for the saga put slot's
// reason, and it calls this runtime's own `invokeIpc`, which no later script can redirect.
(function () {
    Object.defineProperty(globalThis, '__lunaRestoreVolume', {
        value: function (saved) {
            var rust = invokeIpc('connect.get_state').then(
                function (state) { return state && state.outputLevel; },
                function () { return null; }
            );
            return Promise.all([saved, rust]).then(function (both) {
                return both[1] && both[1].chosen === true ? null : both[0];
            });
        },
        writable: false,
        configurable: false,
        enumerable: false
    });
})();
