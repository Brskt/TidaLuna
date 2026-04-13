// Fragment 4/5 - window.open interceptor for auth flows
// Depends on: sendIpc, _cfg (from ipc.js)

var _authHosts = _cfg.authHosts || [];
var origOpen = window.open;
window.open = function(url, target, features) {
    if (target === '_self' && url) {
        for (var i = 0; i < _authHosts.length; i++) {
            if (url.indexOf(_authHosts[i]) !== -1) {
                sendIpc('window.navigate_self', url);
                return window;
            }
        }
    }
    return origOpen.call(window, url, target, features);
};
