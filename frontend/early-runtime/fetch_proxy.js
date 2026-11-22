// window.fetch pinned to the engine's own, with the host's answer observer on it.

var nativeFetch = window.fetch.bind(window);

// The accessor below swallows every assignment; the host cannot wrap window.fetch to watch an
// answer: it takes this slot instead, once, before any plugin runs (same shape as
// __lunaSagaPutInstall). The observer gets (input, answerPromise) of every call. It cannot change
// the answer or fail the call: it is handed the promise the page gets, and what it throws is
// logged, not raised. One that answers a promise holds the page's answer until it settles (see _lunaFetch).
var _fetchObserver = null;
Object.defineProperty(globalThis, '__lunaFetchTapInstall', {
    value: function(h) {
        if (_fetchObserver !== null || typeof h !== 'function') return false;
        _fetchObserver = h;
        return true;
    },
    writable: false,
    configurable: false,
    enumerable: false
});
var _lunaFetch = function(input, init) {
    var answer = nativeFetch(input, init);
    if (_fetchObserver !== null) {
        try {
            var seen = _fetchObserver(input, answer);
            // An observer that answers a promise has something to read off this answer that the page's
            // next step depends on: the page gets the answer once that is done (a rejection of it is the
            // observer's own to report).
            if (seen && typeof seen.then === 'function') {
                return answer.then(function(resp) {
                    var back = function() { return resp; };
                    return seen.then(back, back);
                });
            }
        } catch (e) {
            console.warn('[luna:fetch] observer failed:', e);
        }
    }
    return answer;
};

// Lock window.fetch via accessor descriptor (getter/setter).
// - getter always returns _lunaFetch (the engine's fetch, observed)
// - setter is a no-op - TIDAL's analytics (strict mode) can assign without TypeError
// - configurable:false prevents Object.defineProperty override by plugins
// _lunaFetch only calls nativeFetch (closure-captured above), never window.fetch. A call the engine
// rejects stays rejected: TIDAL retries its own reads, and a replay elsewhere would send a request
// the page never made.
Object.defineProperty(window, 'fetch', {
    get: function() { return _lunaFetch; },
    set: function() {},
    enumerable: true,
    configurable: false
});
