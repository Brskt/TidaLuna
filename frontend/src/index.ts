// this is basically electron's preload script

// --- Bootstrap MUST be first: sets up __ipcRenderer, __platform, window.luna ---
// ESM evaluates imports in declaration order, so this runs before @luna/core.
import "./bootstrap";

import { createApplicationController } from "./controllers/application";
import { createAudioHack } from "./controllers/audio";
import { createNavigationController } from "./controllers/navigation";
import { createPlaybackController } from "./controllers/playback";
import { createUserSession } from "./controllers/session";
import { createUserSettings } from "./controllers/settings";
import { createWindowController } from "./controllers/window";
import { createNativePlayerComponent } from "./controllers/player";
import { initWindowControls } from "./ui/window-controls";
import { invokeIpc } from "./ipc";

// @luna/core and @luna/lib — safe to import after bootstrap
import { initCore, modules, LunaPlugin } from "./luna-core";
import * as LunaCore from "./luna-core";
import * as LunaLib from "./luna-lib";
import * as InrixiaHelpers from "@inrixia/helpers";

// Synchronous initialization: expose nativeInterface immediately so Tidal
// detects desktop mode before its own scripts run.
const credentials: { credentialsStorageKey: string; codeChallenge: string; redirectUri: string; codeVerifier: string } =
    window.__TIDAL_RS_CREDENTIALS__ || {
        credentialsStorageKey: "tidal",
        codeChallenge: "",
        redirectUri: "tidal://auth/",
        codeVerifier: "",
    };

window.nativeInterface = {
    application: createApplicationController(),
    audioHack: createAudioHack(),
    chromecast: undefined,
    credentials,
    features: { chromecast: false, tidalConnect: false },
    navigation: createNavigationController(),
    playback: createPlaybackController(),
    remoteDesktop: undefined,
    tidalConnect: undefined,
    userSession: createUserSession(),
    userSettings: createUserSettings(),
    window: createWindowController(
        window.__TIDAL_RS_WINDOW_STATE__ || { isMaximized: false, isFullscreen: false }
    ),
};
window.NativePlayerComponent = createNativePlayerComponent();
// Bridge event types that map 1:1 (event.t === trigger name, no seq).
const PASSTHROUGH_EVENTS = new Set([
    "devices", "devicedisconnected", "deviceexclusivemodenotallowed",
    "deviceformatnotsupported", "devicelocked", "devicenotfound",
    "deviceunknownerror", "mediaformat", "version", "mediaerror",
    "mediamaxconnectionsreached",
]);
// Short aliases used by the Rust bridge → SDK event names (carry seq).
const SEQ_EVENTS: Record<string, string> = {
    "time": "mediacurrenttime",
    "duration": "mediaduration",
    "state": "mediastate",
};
// Store latest mediaformat from Rust bridge for plugin fallback
// (TidalApi.playbackInfo returns 403 on desktop.tidal.com web tokens)
(window as any).__LUNAR_MEDIA_FORMAT__ = null;
// Promise-based wait for the next mediaformat event (used by updateFormat bridge fallback)
let _mediaFormatResolvers: Array<(data: any) => void> = [];
(window as any).__LUNAR_AWAIT_MEDIA_FORMAT__ = () => new Promise<any>(resolve => {
    console.log("[DBG:format] await registered, pending resolvers:", _mediaFormatResolvers.length + 1);
    _mediaFormatResolvers.push(resolve);
});
// Reset on new load so stale data from previous track isn't used
(window as any).__LUNAR_RESET_MEDIA_FORMAT__ = () => {
    console.log("[DBG:format] FULL RESET — draining", _mediaFormatResolvers.length, "resolvers");
    (window as any).__LUNAR_MEDIA_FORMAT__ = null;
    // Drain pending waiters so they don't receive data from the next track
    for (const r of _mediaFormatResolvers) r(null);
    _mediaFormatResolvers = [];
};

window.__TIDAL_RS_PLAYER_PUSH__ = (events: any[]) => {
    if (!Array.isArray(events)) return;
    const bridge = window.NativePlayerComponent;
    if (!bridge || typeof bridge.trigger !== "function") return;
    for (const event of events) {
        if (!event || typeof event !== "object") continue;
        const type = event.t;
        if (type === "mediaformat") {
            console.log("[DBG:format] mediaformat arrived:", JSON.stringify(event.v), "| pending resolvers:", _mediaFormatResolvers.length);
            (window as any).__LUNAR_MEDIA_FORMAT__ = event.v;
            for (const r of _mediaFormatResolvers) r(event.v);
            _mediaFormatResolvers = [];
        }
        const mapped = SEQ_EVENTS[type];
        if (mapped) {
            bridge.trigger(mapped, event.v, event.seq);
        } else if (PASSTHROUGH_EVENTS.has(type)) {
            bridge.trigger(type, event.v);
        }
    }
};
console.log("Native Interface exposed (sync)");

// --- Bearer token capture: intercept TIDAL API requests to extract OAuth token ---
// Plugins need the token for API calls (getCredentials). TIDAL encrypts it in AuthDB,
// so we capture it from outgoing requests instead.
(window as any).__LUNAR_CAPTURED_TOKEN__ = "";

// Also intercept XMLHttpRequest for token capture (TIDAL may use XHR)
const origXHROpen = XMLHttpRequest.prototype.open;
const origXHRSetHeader = XMLHttpRequest.prototype.setRequestHeader;
XMLHttpRequest.prototype.open = function (this: XMLHttpRequest & { _lunaUrl?: string }, method: string, url: string | URL, ...rest: any[]) {
    this._lunaUrl = typeof url === "string" ? url : url.href;
    return origXHROpen.apply(this, [method, url, ...rest] as any);
};
XMLHttpRequest.prototype.setRequestHeader = function (this: XMLHttpRequest & { _lunaUrl?: string }, name: string, value: string) {
    if (name === "Authorization" && value.startsWith("Bearer ") && this._lunaUrl?.includes("tidal.com")) {
        (window as any).__LUNAR_CAPTURED_TOKEN__ = value.slice(7);
    }
    return origXHRSetHeader.call(this, name, value);
};

// --- CORS proxy: route cross-origin requests through Rust IPC ---
const nativeFetch = window.fetch.bind(window);
const proxyFetch = async (url: string, init?: RequestInit): Promise<Response> => {
    // Convert headers to a plain JSON object for the Rust proxy
    let headersJson = "";
    if (init?.headers) {
        const h = init.headers instanceof Headers ? init.headers : new Headers(init.headers as any);
        const obj: Record<string, string> = {};
        h.forEach((v, k) => { obj[k] = v; });
        if (Object.keys(obj).length > 0) headersJson = JSON.stringify(obj);
    }
    const result = headersJson
        ? await invokeIpc("proxy.fetch", url, headersJson)
        : await invokeIpc("proxy.fetch", url);
    return new Response(result.body, {
        status: result.status,
        headers: { "Content-Type": "application/octet-stream" },
    });
};
window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;

    // Capture Bearer token from TIDAL API requests
    if (url.includes("tidal.com") && init?.headers) {
        const headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers as any);
        const auth = headers.get("Authorization");
        if (auth?.startsWith("Bearer ")) {
            (window as any).__LUNAR_CAPTURED_TOKEN__ = auth.slice(7);
        }
    }

    // Always try native fetch first. Fall back to Rust CORS proxy on TypeError
    // (the error browsers throw on actual CORS blocks).
    try {
        return await nativeFetch(input, init);
    } catch (e) {
        if (!(e instanceof TypeError)) throw e;
        const resp = await proxyFetch(url, init);
        if (resp.status >= 400) console.warn(`[luna:proxy] ${resp.status} ${url.substring(0, 80)}`);
        return resp;
    }
};

const init = async () => {
    const now = Date.now();
    initWindowControls();
    console.log("Native Interface initialized in", Date.now() - now, "ms");

    // --- Discover TIDAL internals (webpack cache, Redux, action creators) ---
    try {
        await initCore();
    } catch (e) {
        console.error("[luna] initCore() failed:", e);
        return;
    }
    console.log("[luna] Core initialized — Redux store discovered, modules populated");

    // --- Push auth credentials to Rust backend for API calls ---
    try {
        const { TidalApi } = await import("./luna-lib/classes/TidalApi");
        await TidalApi.pushCredentialsToRust();
        console.log("[luna] Credentials pushed to Rust backend");
    } catch (e) {
        console.warn("[luna] Failed to push credentials to Rust:", e);
    }

    // --- Register module exports so plugins can require("@luna/core"), etc. ---
    modules["@luna/core"] = LunaCore;
    modules["@luna/lib"] = LunaLib;
    modules["@inrixia/helpers"] = InrixiaHelpers;

    // DBG: wrap onMediaTransition to trace plugin subscriptions
    const _origOMT = LunaLib.MediaItem.onMediaTransition;
    let _omtListenerCount = 0;
    (LunaLib.MediaItem as any).onMediaTransition = (unloads: any, listener: any) => {
        _omtListenerCount++;
        console.log("[DBG:OMT-WRAP] listener registered! count=", _omtListenerCount);
        return _origOMT(unloads, (...args: any[]) => {
            console.log("[DBG:OMT-WRAP] listener CALLED, id=", (args[0] as any)?.id);
            return listener(...args);
        });
    };

    // --- Load @luna/ui core plugin (inline) ---
    try {
        const { LUNA_UI_CODE } = await import("./plugins/luna-ui-inline");
        const blob = new Blob([LUNA_UI_CODE], { type: "application/javascript" });
        const blobUrl = URL.createObjectURL(blob);
        const uiMod = await import(/* @vite-ignore */ blobUrl);
        URL.revokeObjectURL(blobUrl);
        modules["@luna/ui"] = uiMod;
        LunaPlugin.corePlugins.add("@luna/ui");
        console.log("[luna] @luna/ui core plugin loaded");
    } catch (e) {
        console.error("[luna] Failed to load @luna/ui:", e);
    }

    // --- Load all enabled plugins ---
    await LunaPlugin.loadStoredPlugins();
};

setTimeout(init, 0);
