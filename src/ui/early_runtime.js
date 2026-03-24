// TidaLunar Early Runtime — Preload before TIDAL scripts
//
// Injected in on_context_created after cefQuery is installed.
// Sets up the minimum environment TIDAL needs before its own scripts run:
//   - window.fetch with CORS fallback via Rust proxy (cefQuery IPC)
//   - XMLHttpRequest patch for auth token capture
//
// Execution order:
//   1. on_context_created → router installed → THIS SCRIPT
//   2. TIDAL scripts execute (fetch/XHR work via this runtime)
//   3. on_load_start → init_script (TIDAL globals, PKCE, branding)
//   4. on_loading_state_change(is_loading=0) → bundle.js (Luna app)
//
// The bundle does NOT re-override fetch/XHR — it consumes this runtime.

(function() {
    if (self.__LUNAR_EARLY_RUNTIME__) return;
    self.__LUNAR_EARLY_RUNTIME__ = true;
    if (typeof window.cefQuery !== 'function') return;

    // --- Minimal IPC via cefQuery ---

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

    // Expose for bundle consumption
    self.__LUNAR_SEND_IPC__ = sendIpc;
    self.__LUNAR_INVOKE_IPC__ = invokeIpc;

    // --- IPC event system (Rust → JS via eval_js) ---
    // Single shared listener store. The bundle's ipc.ts reuses this instead of creating its own.
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

    // --- Token capture ---

    self.__LUNAR_CAPTURED_TOKEN__ = '';
    function captureToken(token) {
        if (token && token !== self.__LUNAR_CAPTURED_TOKEN__) {
            self.__LUNAR_CAPTURED_TOKEN__ = token;
            sendIpc('jsrt.set_token', token);
        }
    }

    // --- XHR patch: capture Authorization header ---

    var origXHROpen = XMLHttpRequest.prototype.open;
    var origXHRSetHeader = XMLHttpRequest.prototype.setRequestHeader;

    XMLHttpRequest.prototype.open = function(method, url) {
        this._lunaUrl = typeof url === 'string' ? url : url.href;
        return origXHROpen.apply(this, arguments);
    };

    XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
        if (name === 'Authorization' && value.indexOf('Bearer ') === 0 && this._lunaUrl && this._lunaUrl.indexOf('tidal.com') !== -1) {
            captureToken(value.slice(7));
        }
        return origXHRSetHeader.call(this, name, value);
    };

    // --- Fetch proxy: native fetch with CORS fallback via Rust ---

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

    window.fetch = function(input, init) {
        var url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

        // Capture auth token from fetch headers
        if (url.indexOf('tidal.com') !== -1 && init && init.headers) {
            try {
                var h = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
                var auth = h.get('Authorization');
                if (auth && auth.indexOf('Bearer ') === 0) {
                    captureToken(auth.slice(7));
                }
            } catch(e) {}
        }

        // Skip proxy for relative URLs (same-origin, no CORS issue)
        if (url.charAt(0) === '/' && url.charAt(1) !== '/') {
            return nativeFetch(input, init);
        }

        return nativeFetch(input, init).catch(function(err) {
            if (!(err instanceof TypeError)) throw err;
            console.log('[luna:proxy] CORS fallback: ' + url.substring(0, 100));
            return proxyFetch(url, init).then(function(resp) {
                if (resp.status >= 400) console.warn('[luna:proxy] ' + resp.status + ' ' + url.substring(0, 80));
                return resp;
            });
        });
    };

    // --- Normalize window.open for auth flows ---
    var origOpen = window.open;
    window.open = function(url, target, features) {
        console.log('[luna:diag] window.open called: url=' + (url || '(none)') + ' target=' + (target || '(none)'));
        // window.open(url, "_self") and location.assign both cause ERR_ABORTED in CEF
        // for cross-origin auth URLs. Navigate via Rust host instead.
        if (target === '_self' && url && (url.indexOf('login.tidal.com') !== -1 || url.indexOf('auth.tidal.com') !== -1)) {
            console.log('[luna:diag] Routing auth navigation via IPC');
            sendIpc('window.navigate_self', url);
            return window;
        }
        return origOpen.call(window, url, target, features);
    };

    // --- Minimal nativeInterface stub ---
    // TIDAL checks window.nativeInterface to detect desktop mode.
    // The full implementation is set up later by the bundle.
    // This stub ensures TIDAL enters desktop mode from the start.
    if (!window.nativeInterface) {
        var creds = window.__TIDAL_RS_CREDENTIALS__ || {
            credentialsStorageKey: 'tidal',
            codeChallenge: '',
            redirectUri: 'tidal://login/auth',
            codeVerifier: ''
        };
        var noop = function() {};
        var noopObj = { registerDelegate: noop };
        var platform = window.__TIDAL_RS_PLATFORM__ || 'win32';
        // Session delegate stored in a stable global so the bundle can recover it
        // when it replaces nativeInterface.
        self.__LUNAR_SESSION_DELEGATE__ = self.__LUNAR_SESSION_DELEGATE__ || null;
        window.nativeInterface = {
            application: {
                applyUpdate: noop,
                checkForUpdatesSilently: noop,
                getDesktopReleaseNotes: function() { return '{}'; },
                getPlatform: function() { return platform; },
                getPlatformTarget: function() { return 'standalone'; },
                getProcessUptime: function() { return 100; },
                getVersion: function() { return '2.38.6.6'; },
                getWindowsVersionNumber: function() { return Promise.resolve('10.0.0'); },
                ready: noop,
                reenableAutoUpdater: noop,
                registerDelegate: noop,
                reload: function() { window.location.reload(); },
                setWebVersion: noop,
            },
            audioHack: { registerDelegate: noop },
            chromecast: undefined,
            credentials: creds,
            features: { chromecast: false, tidalConnect: false },
            navigation: { registerDelegate: noop },
            playback: {
                registerDelegate: noop,
                setCurrentMediaItem: noop,
                pause: noop,
                play: noop,
                seek: noop,
                setVolume: noop,
            },
            remoteDesktop: undefined,
            tidalConnect: undefined,
            userSession: {
                clear: function() {
                    sendIpc('jsrt.session_clear');
                },
                registerDelegate: function(d) { self.__LUNAR_SESSION_DELEGATE__ = d; },
                update: function(s) {
                    var d = self.__LUNAR_SESSION_DELEGATE__;
                    if (d && typeof d.onSessionChanged === 'function') {
                        d.onSessionChanged(s);
                    }
                },
            },
            userSettings: { registerDelegate: noop },
            window: {
                registerDelegate: noop,
                minimize: noop,
                maximize: noop,
                unmaximize: noop,
                close: noop,
                setFullscreen: noop,
            },
        };

        // Listen for backend ack after session clear completes.
        // Stores a flag so the bundle can detect if clear already happened before it loaded.
        self.__LUNAR_SESSION_CLEAR_DONE__ = false;
        self.__LUNAR_IPC_ON__('jsrt.session_cleared', function() {
            self.__LUNAR_SESSION_CLEAR_DONE__ = true;
            var d = self.__LUNAR_SESSION_DELEGATE__;
            if (d && typeof d.onSessionChanged === 'function') {
                d.onSessionChanged(null);
            }
        });
    }
})();
