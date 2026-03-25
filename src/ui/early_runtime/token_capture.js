// Fragment 2/5 — Bearer token capture from XHR headers
// Depends on: sendIpc (from ipc.js), _cfg (from ipc.js)

self.__LUNAR_CAPTURED_TOKEN__ = '';
function captureToken(token) {
    if (token && token !== self.__LUNAR_CAPTURED_TOKEN__) {
        self.__LUNAR_CAPTURED_TOKEN__ = token;
        sendIpc('jsrt.set_token', token);
    }
}

var _tidalDomain = _cfg.desktopHost || 'tidal.com';
var origXHROpen = XMLHttpRequest.prototype.open;
var origXHRSetHeader = XMLHttpRequest.prototype.setRequestHeader;

XMLHttpRequest.prototype.open = function(method, url) {
    this._lunaUrl = typeof url === 'string' ? url : url.href;
    return origXHROpen.apply(this, arguments);
};

XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
    if (name === 'Authorization' && value.indexOf('Bearer ') === 0 && this._lunaUrl && this._lunaUrl.indexOf(_tidalDomain) !== -1) {
        captureToken(value.slice(7));
    }
    return origXHRSetHeader.call(this, name, value);
};
