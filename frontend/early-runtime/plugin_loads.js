// Plugin loads: one live instance per plugin url, decided by load id
// Wrapped code and unloads carry Rust's rising load id and reach the page in any order: a load
// runs only above every id seen for its url, once the instance it replaces is unloaded.

var _plRecords = Object.create(null);

// The bind it returns gives the instance's unload hook; null means a newer id owns the url.
function _plClaim(url, id) {
    var cur = _plRecords[url];
    if (cur !== undefined && cur.id >= id) return null;
    if (cur !== undefined && cur.unload) {
        var prev = cur.unload;
        cur.unload = null;
        try { prev(); } catch (e) { console.error('[plugin:unload]', url, e); }
    }
    var rec = { id: id, unload: null };
    _plRecords[url] = rec;
    return function(fn) {
        if (rec.id === id && typeof fn === 'function') rec.unload = fn;
    };
}

// Unloads the instance of this id or older. Arriving before its load, it leaves the id behind
// so that load is refused.
function _plUnload(url, id) {
    var rec = _plRecords[url];
    if (rec === undefined) {
        _plRecords[url] = { id: id, unload: null };
        return;
    }
    if (rec.id > id) return;
    var hook = rec.unload;
    rec.id = id;
    rec.unload = null;
    if (hook) {
        try { hook(); } catch (e) { console.error('[plugin:unload]', url, e); }
    }
}

// Frozen and fixed: a plugin can call these, not replace them or reach the records.
Object.defineProperty(self, '__pluginLoads', {
    value: Object.freeze({ claim: _plClaim, unload: _plUnload })
});
