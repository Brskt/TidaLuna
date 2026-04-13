// Fragment 1/5 - IPC via cefQuery + event bus
// Guard (__LUNAR_EARLY_RUNTIME__) and cefQuery check are in the Rust preload assembler.

// Capture cefQuery once at early-runtime init - before any plugin or app code runs.
// sendIpc/invokeIpc use this private reference, so patching window.cefQuery later
// (e.g., by a plugin) cannot intercept jsrt.set_token or other early-runtime IPC.
var _cq = window.cefQuery;
var _cfg = self.__LUNAR_CONFIG__ || {};
var _ipcId = 0;

function sendIpc(channel, arg) {
    var args = arg !== undefined ? [arg] : [];
    _cq({
        request: JSON.stringify({ channel: channel, args: args }),
        onSuccess: function() {},
        onFailure: function(code, msg) {
            console.error('[luna:ipc] FAIL:', channel, code, msg);
        }
    });
}

function invokeIpc(channel) {
    var args = [];
    for (var i = 1; i < arguments.length; i++) args.push(arguments[i]);
    return new Promise(function(resolve, reject) {
        var id = String(++_ipcId);
        _cq({
            request: JSON.stringify({ channel: channel, args: args, id: id }),
            onSuccess: function(response) {
                if (response.indexOf('E:') === 0) {
                    reject(new Error(response.slice(2)));
                } else if (response.indexOf('S:') === 0) {
                    try { resolve(JSON.parse(response.slice(2))); }
                    catch(e) { resolve(response.slice(2)); }
                } else {
                    resolve(response);
                }
            },
            onFailure: function(code, msg) {
                reject(new Error('[IPC] ' + channel + ': ' + msg + ' (' + code + ')'));
            }
        });
    });
}

var listeners = self.__LUNAR_IPC_LISTENERS__ = self.__LUNAR_IPC_LISTENERS__ || {};
self.__LUNAR_IPC_EMIT__ = function(channel) {
    var args = [];
    for (var i = 1; i < arguments.length; i++) args.push(arguments[i]);
    var cbs = listeners[channel];
    if (!cbs) return;
    for (var j = 0; j < cbs.length; j++) {
        try { cbs[j].apply(null, args); } catch(e) {}
    }
};
self.__LUNAR_IPC_ON__ = function(channel, callback) {
    if (!listeners[channel]) listeners[channel] = [];
    listeners[channel].push(callback);
};
