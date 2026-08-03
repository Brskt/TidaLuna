// Reuse the shared listener store created by early_runtime.js.

const sharedListeners: Record<string, Array<(...args: any[]) => void>> =
    (window as any).__LUNAR_IPC_LISTENERS__ ??= {};

(window as any).__LUNAR_IPC_EMIT__ = (channel: string, ...args: any[]) => {
    const cbs = sharedListeners[channel];
    if (!cbs) return;
    for (const cb of cbs) {
        try {
            cb(...args);
        } catch (e) {
            console.error("[IPC] Event handler error:", channel, e);
        }
    }
};

// --- Public API ---

// Captured once. Re-reading window.cefQuery per call let the first plugin to run redirect every
// later call, for every plugin, for the rest of the session. Safe by construction: the app bundle
// runs before any plugin, since it is what asks Rust to load them.
const cefQuery = window.cefQuery;

// Unattributed on purpose: `@luna/lib` imports these, and plugin code reaches the lib through
// `window.luna.lib`; a capability here would be handed to whoever asked. Attributed calls are
// built in the per-plugin wrapper (`src/plugins/wrapper.rs`), the only scope holding one.
export const sendIpc = (channel: string, ...args: any[]) => {
    cefQuery({
        request: JSON.stringify({ channel, args }),
        onSuccess: () => {},
        onFailure: (code: number, msg: string) => console.error("[IPC] FAIL:", channel, code, msg),
    });
};

// Diagnostic `player.dbg` IPC, gated on the Rust log level mirrored into
// `window.__TIDALUNAR_LOG_LEVEL__` (>= 2 matches vprintln2!). Below that the message
// never crosses the renderer->browser boundary, instead of Rust discarding it after.
export const sendDbgIpc = (...args: any[]) => {
    if (Number((window as any).__TIDALUNAR_LOG_LEVEL__ ?? 0) >= 2) sendIpc("player.dbg", ...args);
};

let invokeCounter = 0;

export const invokeIpc = (channel: string, ...args: any[]): Promise<any> => {
    return new Promise((resolve, reject) => {
        const id = `${++invokeCounter}`;
        cefQuery({
            request: JSON.stringify({ channel, args, id }),
            onSuccess: (response: string) => {
                try {
                    resolve(JSON.parse(response));
                } catch {
                    resolve(response);
                }
            },
            onFailure: (code: number, msg: string) => {
                reject(Object.assign(new Error(msg), { code, channel }));
            },
        });
    });
};

export const onIpcEvent = (channel: string, callback: (...args: any[]) => void): (() => void) => {
    if (!sharedListeners[channel]) sharedListeners[channel] = [];
    sharedListeners[channel].push(callback);
    return () => {
        const cbs = sharedListeners[channel];
        if (cbs) {
            const idx = cbs.indexOf(callback);
            if (idx !== -1) cbs.splice(idx, 1);
        }
    };
};

export const isLoginCallback = (): boolean =>
    window.location.pathname === ((window as any).__LUNAR_CONFIG__?.loginCallbackPath ?? "/login/auth");

/**
 * Authenticated fetch restricted to TIDAL API hosts.
 * Routes through Rust via `tidal.fetch` IPC - the OAuth token is injected
 * server-side and never exposed to JavaScript.
 */
export interface TidalFetchResponse {
	ok: boolean;
	status: number;
	statusText: string;
	url: string;
	headers: Record<string, string>;
	json<T = any>(): Promise<T>;
	text(): Promise<string>;
}
export const tidalFetch = async (url: string, init?: { method?: string; headers?: Record<string, string>; body?: string }): Promise<TidalFetchResponse> => {
	const opts: Record<string, unknown> = {};
	if (init?.method) opts.method = init.method;
	if (init?.headers && Object.keys(init.headers).length > 0) opts.headers = init.headers;
	if (init?.body) opts.body = init.body;
	const optsJson = Object.keys(opts).length > 0 ? JSON.stringify(opts) : "{}";
	const raw: { ok: boolean; status: number; statusText: string; url: string; headers: Record<string, string>; body: string } = await invokeIpc("tidal.fetch", url, optsJson);
	return {
		ok: raw.ok,
		status: raw.status,
		statusText: raw.statusText,
		url: raw.url,
		headers: raw.headers ?? {},
		json: <T = any>() => Promise.resolve(JSON.parse(raw.body) as T),
		text: () => Promise.resolve(raw.body),
	};
};

(window as any).__LUNAR_IPC_ON__ = (channel: string, cb: (...args: any[]) => void) => {
    onIpcEvent(channel, cb);
};
