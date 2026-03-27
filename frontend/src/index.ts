// Bootstrap MUST be first — sets up globals before @luna/core imports.
import "./bootstrap";

import { createApplicationController } from "./controllers/application";
import { createAudioHack } from "./controllers/audio";
import { createNavigationController } from "./controllers/navigation";
import { createPlaybackController } from "./controllers/playback";
import { createUserSession } from "./controllers/session";
import { createUserSettings } from "./controllers/settings";
import { createWindowController } from "./controllers/window";
import { createNativePlayerComponent } from "./controllers/player";
import { updatePlaybackState } from "./controllers/mediasession";
import { proxySetPlaying, proxySetTime, proxySetDuration, proxyReset, isSelfLoad } from "./audio-proxy";
import { initWindowControls } from "./ui/window-controls";
import { invokeIpc, sendIpc, isLoginCallback } from "./ipc";

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
        redirectUri: (window as any).__LUNAR_CONFIG__?.redirectUri ?? "tidal://login/auth",
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
let _lastTimeDispatch = 0;
let _forceTimeDispatch = false;

// Short aliases used by the Rust bridge → SDK event names (carry seq).
const SEQ_EVENTS: Record<string, string> = {
    "time": "mediacurrenttime",
    "duration": "mediaduration",
    "state": "mediastate",
};
const BRIDGE_TO_REDUX_STATE: Record<string, string> = {
    "active": "PLAYING",
    "paused": "NOT_PLAYING",
    "completed": "IDLE",
    "idle": "IDLE",
};
// Mediaformat bridge: latest format data from Rust player (playbackInfo fallback)
(window as any).__LUNAR_MEDIA_FORMAT__ = null;
let _mediaFormatResolvers: Array<(data: any) => void> = [];
(window as any).__LUNAR_AWAIT_MEDIA_FORMAT__ = () => new Promise<any>(resolve => {
    _mediaFormatResolvers.push(resolve);
});
(window as any).__LUNAR_RESET_MEDIA_FORMAT__ = () => {
    (window as any).__LUNAR_MEDIA_FORMAT__ = null;
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
            (window as any).__LUNAR_MEDIA_FORMAT__ = event.v;
            for (const r of _mediaFormatResolvers) r(event.v);
            _mediaFormatResolvers = [];
        }
        if (type === "time") {
            proxySetTime(event.v);
            const now = Date.now();
            if (_forceTimeDispatch || now - _lastTimeDispatch >= 250) {
                _forceTimeDispatch = false;
                _lastTimeDispatch = now;
                try {
                    const { store } = require("./luna-lib/redux/store");
                    store.dispatch({ type: "playbackControls/TIME_UPDATE", payload: event.v });
                } catch (_) {}
            }
        } else if (type === "duration") {
            proxySetDuration(event.v);
        } else if (type === "volume") {
            try {
                const { store } = require("./luna-lib/redux/store");
                store.dispatch({
                    type: "playbackControls/SET_VOLUME",
                    payload: { volume: Math.round(event.v) },
                });
            } catch (_) {}
        } else if (type === "state") {
            const playing = event.v === "active";
            (window as any).__TL_PLAYING__ = playing;
            updatePlaybackState(playing);
            if (event.v === "completed") {
                proxyReset();
                if (isSelfLoad()) {
                    // Self-loaded tracks bypass NativePlayer — manually advance queue.
                    const { store } = require("./luna-lib/redux/store");
                    const { playQueue: q } = store.getState();
                    const nextId = q.elements[q.currentIndex + 1]?.mediaItemId;
                    if (nextId) {
                        setTimeout(() => {
                            try {
                                store.dispatch({ type: "playQueue/MOVE_NEXT" });
                                window.nativeInterface.playback.setCurrentMediaItem({
                                    productId: nextId,
                                    type: "track",
                                });
                            } catch (e) {
                                console.error("[luna] DASH auto-advance failed:", e);
                            }
                        }, 0);
                    }
                }
            } else {
                proxySetPlaying(playing);
            }
            const reduxState = BRIDGE_TO_REDUX_STATE[event.v as string];
            if (reduxState) {
                try {
                    const { store } = require("./luna-lib/redux/store");
                    store.dispatch({ type: "playbackControls/SET_PLAYBACK_STATE", payload: reduxState });
                } catch (_) {}
            }
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

// Fetch proxy, XHR patch, and token capture live in early_runtime.js (on_context_created).

const init = async () => {
    const now = Date.now();
    initWindowControls();
    console.log("Native Interface initialized in", Date.now() - now, "ms");

    if (isLoginCallback()) return;

    try {
        await initCore();
    } catch (e) {
        console.error("[luna] initCore() failed:", e);
        return;
    }
    console.log("[luna] Core initialized — Redux store discovered, modules populated");

    // SDK middleware doesn't reach Rust player for DASH/AAC — intercept Redux actions.
    {
        const { interceptors } = require("./luna-core/exposeTidalInternals.patchAction");
        const add = (action: string, cb: Function) => {
            interceptors[action] ??= new Set();
            interceptors[action].add(cb);
        };
        add("playbackControls/PLAY", () => {
            if (isSelfLoad()) sendIpc("player.play");
        });
        add("playbackControls/SEEK", (time: number) => {
            sendIpc("player.seek", time);
            _forceTimeDispatch = true;
        });
        add("playbackControls/SET_VOLUME", (p: { volume: number }) => sendIpc("player.volume", p.volume));
    }

    modules["@luna/core"] = LunaCore;
    modules["@luna/lib"] = LunaLib;
    modules["@inrixia/helpers"] = InrixiaHelpers;

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

    try {
        const { LUNA_DEV_CODE } = await import("./plugins/luna-dev-inline");
        const blob = new Blob([LUNA_DEV_CODE], { type: "application/javascript" });
        const blobUrl = URL.createObjectURL(blob);
        const devMod = await import(/* @vite-ignore */ blobUrl);
        URL.revokeObjectURL(blobUrl);
        modules["@luna/dev"] = devMod;
        LunaPlugin.corePlugins.add("@luna/dev");
        console.log("[luna] @luna/dev core plugin loaded");
    } catch (e) {
        console.error("[luna] Failed to load @luna/dev:", e);
    }

    // Load user plugins — Rust prepares + wraps them, injects into CEF
    sendIpc("jsrt.load_plugins");
    // Populate CEF plugin registry from Rust PluginStore (for @luna/ui display)
    const { exposeLoaderApi } = require("./plugins/loader");
    exposeLoaderApi();
    console.log("[luna] Plugin execution delegated to Rust plugin manager");
};

setTimeout(init, 0);
