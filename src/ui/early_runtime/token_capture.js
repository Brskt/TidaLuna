// Fragment 2/5 — Bearer token capture from XHR headers
// Depends on: sendIpc (from ipc.js), _cfg (from ipc.js)

var _lastToken = '';
function captureToken(token) {
    if (token && token !== _lastToken && token.indexOf('luna_') !== 0) {
        _lastToken = token;
        sendIpc('jsrt.set_token', token);
    }
}

var _tidalDomain = _cfg.desktopHost || 'tidal.com';
var origXHROpen = XMLHttpRequest.prototype.open;
var origXHRSetHeader = XMLHttpRequest.prototype.setRequestHeader;

var _patchedOpen = function(method, url) {
    this._lunaUrl = typeof url === 'string' ? url : url.href;
    return origXHROpen.apply(this, arguments);
};

var _patchedSetHeader = function(name, value) {
    if (name === 'Authorization' && value.indexOf('Bearer ') === 0 && this._lunaUrl && this._lunaUrl.indexOf(_tidalDomain) !== -1) {
        captureToken(value.slice(7));
    }
    return origXHRSetHeader.call(this, name, value);
};

// Lock XHR prototype methods via accessor descriptors (getter/setter).
// - getter always returns our patched version (token capture + chain to native)
// - setter is a no-op — TIDAL's libs (strict mode) can assign without TypeError
// - configurable:false prevents Object.defineProperty override by plugins
// This protects against prototype-level re-patching. Instance-level patches
// (xhr.setRequestHeader = ...) on a specific XHR object are not blocked,
// but require the attacker to have a reference to TIDAL's internal XHR instances.
// Lock the XMLHttpRequest constructor on window — prevents a plugin from replacing
// it with a subclass/wrapper that instruments instances before they reach our prototype patches.
var _XHR = XMLHttpRequest;
Object.defineProperty(window, 'XMLHttpRequest', {
    get: function() { return _XHR; },
    set: function() {},
    enumerable: true,
    configurable: false
});
Object.defineProperty(XMLHttpRequest.prototype, 'open', {
    get: function() { return _patchedOpen; },
    set: function() {},
    enumerable: true,
    configurable: false
});
Object.defineProperty(XMLHttpRequest.prototype, 'setRequestHeader', {
    get: function() { return _patchedSetHeader; },
    set: function() {},
    enumerable: true,
    configurable: false
});
