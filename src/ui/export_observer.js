// Export Observer — injected via on_context_created before any page scripts.
//
// Hooks Object.defineProperty (and Reflect.defineProperty) to detect webpack's
// __webpack_require__.d pattern: enumerable getter-only property definitions.
// For each such definition, the specific getter is queued and evaluated in a
// deferred microtask to check if the exported value is an RTK action creator.
// Only the defined getter is invoked — no unrelated properties are read.
//
// The registry is always accessed via self.__LUNAR_BUILD_ACTIONS__ (never a
// closed-over variable) so the frontend can rebind it after init and late
// discoveries still land in the right object.
//
// This works even when webpack runs inside a closed IIFE with no globals,
// because Object.defineProperty is a global that we patch before the bundle.
(function() {
    if (self.__LUNAR_EXPORT_OBSERVER__) return;
    self.__LUNAR_EXPORT_OBSERVER__ = true;

    self.__LUNAR_BUILD_ACTIONS__ = {};
    var pendingGetters = [];
    var scheduled = false;
    var count = 0;

    function isSimpleCreator(val) {
        return typeof val === 'function'
            && typeof val.type === 'string'
            && typeof val.match === 'function';
    }

    function isThunkCreator(val) {
        return typeof val === 'function'
            && typeof val.typePrefix === 'string'
            && typeof val.pending === 'function'
            && typeof val.fulfilled === 'function'
            && typeof val.rejected === 'function';
    }

    function flush() {
        scheduled = false;
        var batch = pendingGetters;
        pendingGetters = [];
        var reg = self.__LUNAR_BUILD_ACTIONS__;
        for (var i = 0; i < batch.length; i++) {
            try {
                var val = batch[i]();
                if (isSimpleCreator(val) && !reg[val.type]) {
                    reg[val.type] = val;
                    count++;
                } else if (isThunkCreator(val) && !reg[val.typePrefix]) {
                    reg[val.typePrefix] = val;
                    count++;
                }
            } catch(e) {}
        }
    }

    function enqueue(getter) {
        pendingGetters.push(getter);
        if (!scheduled) {
            scheduled = true;
            Promise.resolve().then(flush);
        }
    }

    var origDefineProperty = Object.defineProperty;
    Object.defineProperty = function(target, prop, descriptor) {
        var result = origDefineProperty.call(this, target, prop, descriptor);
        if (descriptor && typeof descriptor.get === 'function' && descriptor.enumerable === true) {
            enqueue(descriptor.get);
        }
        return result;
    };

    if (typeof Reflect !== 'undefined' && Reflect.defineProperty) {
        var origReflectDefine = Reflect.defineProperty;
        Reflect.defineProperty = function(target, prop, descriptor) {
            var result = origReflectDefine.call(this, target, prop, descriptor);
            if (descriptor && typeof descriptor.get === 'function' && descriptor.enumerable === true) {
                enqueue(descriptor.get);
            }
            return result;
        };
    }

    // Log results after TIDAL bundle has likely finished executing
    setTimeout(function() {
        var reg = self.__LUNAR_BUILD_ACTIONS__;
        var types = Object.keys(reg);
        console.log('[luna:observer] Discovered ' + count + ' action creators via export observation');
        if (types.length > 0) {
            console.log('[luna:observer] Types: ' + types.slice(0, 20).join(', ') + (types.length > 20 ? ' ... (' + types.length + ' total)' : ''));
        }
    }, 5000);
})();
