// Fragment 1/5 — IPC via cefQuery + event bus
// Wrapped in a single IIFE by the Rust preload assembler.

if (self.__LUNAR_EARLY_RUNTIME__) return;
self.__LUNAR_EARLY_RUNTIME__ = true;
if (typeof window.cefQuery !== 'function') return;

var _cfg = self.__LUNAR_CONFIG__ || {};
var _ipcId = 0;

function sendIpc(channel, arg) {
    var args = arg !== undefined ? [arg] : [];
    window.cefQuery({
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
        window.cefQuery({
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

self.__LUNAR_SEND_IPC__ = sendIpc;
self.__LUNAR_INVOKE_IPC__ = invokeIpc;

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
