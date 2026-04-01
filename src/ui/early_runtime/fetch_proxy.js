// Fragment 3/5 — Fetch proxy with CORS fallback via Rust
// Depends on: invokeIpc, captureToken, _tidalDomain (from previous fragments)

var nativeFetch = window.fetch.bind(window);

function serializeBody(body) {
    if (body == null) return undefined;
    if (typeof body === 'string') return body;
    if (body instanceof URLSearchParams) return body.toString();
    if (body instanceof ArrayBuffer) return new TextDecoder().decode(body);
    if (body instanceof Uint8Array) return new TextDecoder().decode(body);
    if (ArrayBuffer.isView(body)) return new TextDecoder().decode(body.buffer);
    return undefined;
}

function proxyFetch(url, init) {
    var opts = {};
    if (init && init.method && init.method.toUpperCase() !== 'GET') {
        opts.method = init.method.toUpperCase();
    }
    if (init && init.headers) {
        try {
            var h = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
            var obj = {};
            h.forEach(function(v, k) { obj[k] = v; });
            if (Object.keys(obj).length > 0) opts.headers = JSON.stringify(obj);
        } catch(e) {}
    }
    var body = init ? serializeBody(init.body) : undefined;
    if (body !== undefined) opts.body = body;

    var optsJson = Object.keys(opts).length > 0 ? JSON.stringify(opts) : '';
    var ipcArgs = optsJson ? [url, optsJson] : [url];

    return invokeIpc.apply(null, ['proxy.fetch'].concat(ipcArgs)).then(function(result) {
        var respHeaders = result.headers || {};
        return new Response(result.body || '', {
            status: result.status || 200,
            headers: respHeaders
        });
    });
}

var _lunaFetch = function(input, init) {
    var url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

    if (url.indexOf(_tidalDomain) !== -1 && init && init.headers) {
        try {
            var h = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
            var auth = h.get('Authorization');
            if (auth && auth.indexOf('Bearer ') === 0) {
                captureToken(auth.slice(7));
            }
        } catch(e) {}
    }

    if (url.charAt(0) === '/' && url.charAt(1) !== '/') {
        return nativeFetch(input, init);
    }

    return nativeFetch(input, init).catch(function(err) {
        if (!(err instanceof TypeError)) throw err;
        // Only proxy *.tidal.com — Rust rejects non-Tidal URLs on proxy.fetch.
        if (url.indexOf('tidal.com') === -1) throw err;
        return proxyFetch(url, init).then(function(resp) {
            if (resp.status >= 400) console.warn('[luna:proxy] ' + resp.status + ' ' + url.substring(0, 80));
            return resp;
        });
    });
};

// Lock window.fetch via accessor descriptor (getter/setter).
// - getter always returns _lunaFetch (our patched version with token capture)
// - setter is a no-op — TIDAL's analytics (strict mode) can assign without TypeError
// - configurable:false prevents Object.defineProperty override by plugins
// _lunaFetch only calls nativeFetch (closure-captured above), never window.fetch.
Object.defineProperty(window, 'fetch', {
    get: function() { return _lunaFetch; },
    set: function() {},
    enumerable: true,
    configurable: false
});
