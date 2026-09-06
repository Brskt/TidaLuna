// Bootstrap MUST be first - sets up globals before @luna/core imports.
import "./bootstrap";

import { createApplicationController } from "./controllers/application";
import { createAudioHack } from "./controllers/audio";
import { createNavigationController } from "./controllers/navigation";
import { createPlaybackController } from "./controllers/playback";
import { createUserSession } from "./controllers/session";
import { createUserSettings } from "./controllers/settings";
import { createWindowController } from "./controllers/window";
import { createTidalConnectController, createRemoteDesktopController, setupConnectEventListeners } from "./connect";
import { createNativePlayerComponent } from "./controllers/player";
import { updatePlaybackState } from "./controllers/mediasession";
import { proxySetPlaying, proxySetTime, proxySetDuration, proxyReset, proxyFail, isSelfLoad } from "./audio-proxy";
import { refusePlayback } from "./refuse-playback";
import { initWindowControls } from "./ui/window-controls";
import { invokeIpc, invokeIpcAs, sendIpc, sendIpcAs, isLoginCallback, onIpcEvent } from "./ipc";
import { initPerfOverlay } from "./debug/perf-overlay";

// @luna/core and @luna/lib - safe to import after bootstrap
import { defineHostModule, initCore, modules, LunaPlugin } from "../render/src";
import * as LunaCore from "../render/src";
import * as LunaLib from "../plugins/lib/src";
import * as InrixiaHelpers from "@inrixia/helpers";
import * as LibNative from "../plugins/lib.native/src/index.native";

// Synchronous initialization: expose nativeInterface immediately for Tidal
// to detect desktop mode before its own scripts run.
const credentials: { credentialsStorageKey: string; codeChallenge: string; redirectUri: string; codeVerifier: string } =
    window.__TIDALUNAR_CREDENTIALS__ || {
        credentialsStorageKey: "tidal",
        codeChallenge: "",
        redirectUri: (window as any).__LUNAR_CONFIG__?.redirectUri ?? "tidal://login/auth",
        codeVerifier: "",
    };
delete window.__TIDALUNAR_CREDENTIALS__;

window.nativeInterface = {
    application: createApplicationController(),
    audioHack: createAudioHack(),
    chromecast: undefined,
    credentials,
    features: { chromecast: false, tidalConnect: true, remoteDesktop: true },
    navigation: createNavigationController(),
    playback: createPlaybackController(),
    remoteDesktop: createRemoteDesktopController(),
    tidalConnect: createTidalConnectController(),
    userSession: createUserSession(),
    userSettings: createUserSettings(),
    window: createWindowController(
        window.__TIDALUNAR_WINDOW_STATE__ || { isMaximized: false, isFullscreen: false }
    ),
};
window.NativePlayerComponent = createNativePlayerComponent();

// Wire up TIDAL Connect event listeners once the store is available
try {
    const { store } = require("../plugins/lib/src/redux/store");
    setupConnectEventListeners(store);
} catch {
    // Store not yet available - listeners will be set up when Redux initializes
}

// Live perf overlay (debug tool) - active only when Rust set the flag (env TIDALUNAR_PERF).
if ((window as { __TIDALUNAR_PERF__?: boolean }).__TIDALUNAR_PERF__) {
    initPerfOverlay();
}

// Bridge event types that map 1:1 (event.t === trigger name, no seq).
const PASSTHROUGH_EVENTS = new Set([
    "devices", "devicedisconnected", "deviceexclusivemodenotallowed",
    "deviceformatnotsupported", "devicelocked", "devicenotfound",
    "deviceunknownerror", "mediaformat", "version", "mediaerror",
    "mediamaxconnectionsreached",
    "deviceasiodrivernotfound", "deviceasioformatunsupported",
    "deviceasioinitfailed", "deviceasiorateunsupported",
    "deviceexclusiveformatunsupported",
]);
let _lastTimeDispatch = 0;
let _forceTimeDispatch = false;
// Let the load delegate (player.ts) bypass the 250ms time throttle for the
// first report of a fresh load: the bar snaps to the new track's start
// instead of holding the previous track's position (mirrors the SEEK arm).
(window as any).__LUNAR_FORCE_TIME_DISPATCH__ = () => {
    _forceTimeDispatch = true;
};

// Short aliases used by the Rust bridge -> SDK event names (carry seq).
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

window.__TIDALUNAR_PLAYER_PUSH__ = (events: any[]) => {
    if (!Array.isArray(events)) return;
    const bridge = window.NativePlayerComponent;
    if (!bridge || typeof bridge.trigger !== "function") return;
    // A batch carrying a track change carries the new track's first position behind
    // it. That position is the one report the 250ms throttle must not drop: the
    // window it would wait out was armed by the OUTGOING track's last tick. Left
    // alone, the bar keeps rendering the old position under the new track. Armed
    // before the loop rather than from the "completed" arm; the order of the two in
    // the batch does not matter. Same carve-out load() and SEEK already take,
    // applied to the transition, which goes through neither.
    //
    // Both halves are required. Arming on "completed" alone leaks: a natural track
    // end in exclusive or ASIO output sends the state with no position beside it,
    // and the flag would then sit armed until some unrelated later report spent it.
    let _hasTransition = false;
    let _hasTime = false;
    for (const e of events) {
        if (!e || typeof e !== "object") continue;
        if (e.t === "time") _hasTime = true;
        else if (e.t === "state" && e.v === "completed") _hasTransition = true;
    }
    if (_hasTransition && _hasTime) {
        _forceTimeDispatch = true;
    }
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
                    const { store } = require("../plugins/lib/src/redux/store");
                    store.dispatch({ type: "playbackControls/TIME_UPDATE", payload: event.v });
                } catch (_) {}
            }
        } else if (type === "duration") {
            proxySetDuration(event.v);
        } else if (type === "volume") {
            try {
                const { store } = require("../plugins/lib/src/redux/store");
                (bridge as any).syncBridgeVolume?.(Math.round(event.v));
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
                    // Self-loaded tracks bypass NativePlayer - manually advance queue.
                    const { store } = require("../plugins/lib/src/redux/store");
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
                    const { store } = require("../plugins/lib/src/redux/store");
                    store.dispatch({ type: "playbackControls/SET_PLAYBACK_STATE", payload: reduxState });
                } catch (_) {}
            }
        }
        const mapped = SEQ_EVENTS[type];
        if (type === "medianetworklost") {
            // Not a passthrough: a raw `mediaerror` loses TIDAL's one-second STALLED
            // race, leaving the spinner turning over a player that will never resume.
            // Rust has already waited thirty seconds for the bytes; this is the answer
            // to their never arriving, and it must not advance the queue.
            refusePlayback(
                "tidalunar_network_lost",
                "no bytes for 30s",
                "TidaLunar cannot play music without an internet connection. Please try again when you're connected.",
            );
        } else if (mapped) {
            bridge.trigger(mapped, event.v, event.seq);
        } else if (PASSTHROUGH_EVENTS.has(type)) {
            bridge.trigger(type, event.v);
            if (type === "mediaerror") {
                // Rust has published this since the bridge was written and it stopped at the
                // SDK delegate, leaving the element it describes unaware. Kept deliberately:
                // this is how player-side errors reach the client.
                proxyFail();
            }
            if (type === "deviceexclusivemodenotallowed") {
                // Permanent: the device doesn't support exclusive mode. Neither
                // the native player nor the SDK reverts the store's mode; the
                // toggle stays stuck on (Rust falls back to shared). Re-select
                // shared for activeDeviceMode to resync. NOT for the transient
                // 'devicelocked' case: flipping it there would never re-arm
                // exclusive once the device frees.
                try {
                    const { store } = require("../plugins/lib/src/redux/store");
                    store.dispatch({ type: "player/SET_DEVICE_MODE", payload: "shared" });
                } catch (_) {}
                // Clear the persisted exclusive flag too: a restart doesn't re-seed
                // exclusive and re-enter the failing path (mirrors the ASIO failure clear).
                (window as any).__TIDALUNAR_EXCLUSIVE__ = false;
                sendIpc("settings.exclusive", false);
            }
            if (
                type === "deviceasiodrivernotfound" ||
                type === "deviceasioformatunsupported" ||
                type === "deviceasioinitfailed"
            ) {
                // ASIO failed; Rust fell back to shared output. Clear the flag AND persist it,
                // keeping a restart from re-seeding ASIO and re-entering the failing path.
                (window as any).__TIDALUNAR_ASIO__ = false;
                sendIpc("settings.asio", false);
            }
        }
    }
};
console.log("Native Interface exposed (sync)");

// Fetch proxy, XHR patch, and token capture live in early_runtime.js (on_context_created).

const init = async () => {
    const now = Date.now();
    initWindowControls();
    console.log("Native Interface initialized in", Date.now() - now, "ms");

    if (isLoginCallback()) {
        // Bundle runs on /login/auth but TIDAL's React app isn't mounted yet.
        // Rust emits jsrt.post_login_init when the SPA navigates to the app.
        const unsub = onIpcEvent("jsrt.post_login_init", () => {
            unsub();
            setTimeout(init, 0);
        });
        return;
    }

    try {
        await initCore();
    } catch (e) {
        console.error("[luna] initCore() failed:", e);
        return;
    }
    console.log("[luna] Core initialized - Redux store discovered, modules populated");

    // Hydrate close-to-tray from Rust-persisted value and expose setter for Rust
    {
        const { store } = require("../plugins/lib/src/redux/store");
        const setCloseToTray = (enabled: boolean) => {
            const desktop = store.getState().settings?.desktop;
            if (desktop && desktop.closeToTray !== enabled) {
                store.dispatch({
                    type: "settings/DESKTOP_SETTINGS_UPDATED",
                    payload: { ...desktop, closeToTray: enabled },
                });
            }
        };
        (window as any).__LUNAR_SET_CLOSE_TO_TRAY__ = setCloseToTray;
        setCloseToTray(!!(window as any).__TIDALUNAR_CLOSE_TO_TRAY__);
    }

    // SDK middleware doesn't reach Rust player for DASH/AAC - intercept Redux actions.
    {
        const { interceptors } = require("../render/src/exposeTidalInternals.patchAction");
        const add = (action: string, cb: Function) => {
            interceptors[action] ??= new Set();
            interceptors[action].add(cb);
        };
        add("playbackControls/PLAY", () => {
            // Self-load (DASH/AAC) keeps its direct path; SDK tracks go through the
            // deduped desired-intent: a user PLAY reaches Rust even when TIDAL
            // resumes via stop+load(same) without calling nativePlayer.play().
            if (isSelfLoad()) sendIpc("player.play");
            else (window.NativePlayerComponent as any)?.setDesiredPlayback?.(true);
        });
        add("playbackControls/PAUSE", () => {
            if (!isSelfLoad()) (window.NativePlayerComponent as any)?.setDesiredPlayback?.(false);
        });
        // TIDAL's "play this" signal fires BEFORE the selected track's load (async
        // getPlaybackInfo gap) while the OLD track is still committed; resuming here audibly
        // replays the OLD track (the "bleed"). Request auto-play on the SELECTED track's
        // load instead of a separate play: fixes the bleed and the startup first-play.
        add("playQueue/ADD_TRACK_LIST_TO_PLAY_QUEUE", (p: { position?: number | string }) => {
            // Only "now" carries play intent; "next"/"last"/index are queue edits with no
            // load following, and arming on them would force-resume a paused track later.
            // Unknown payload shapes keep the arm (HW-verified row-click sends {position:"now"}).
            const position = p?.position;
            if (position === undefined || position === "now") {
                (window.NativePlayerComponent as any)?.requestPlayOnLoad?.();
            }
        });
        add("playbackControls/SEEK", (time: number) => {
            sendIpc("player.seek", time);
            _forceTimeDispatch = true;
        });
        // Volume: for self-loaded tracks (DASH/AAC) the SDK doesn't call setVolume;
        // we forward Redux SET_VOLUME to Rust directly.
        add("playbackControls/SET_VOLUME", (p: { volume: number }) => {
            if (isSelfLoad()) sendIpc("player.volume", p.volume);
        });
        add("settings/TOGGLE_CLOSE_TO_TRAY", () => {
            const { store } = require("../plugins/lib/src/redux/store");
            const current = store.getState().settings?.desktop?.closeToTray ?? false;
            sendIpc("settings.close_to_tray", !current);
        });
    }

    // Pinned: it is the second link of the chain a plugin's `@luna/lib` import is lowered to
    // (`luna.core.modules["__lunaLibFor"]`), and the accessor in front of it only forwards
    // here. Left writable, replacing THIS slot bypassed the pin on `__lunaLibFor` entirely.
    // `@luna/lib` below stays an ordinary slot: it is a core-plugin name, and `LunaPlugin`
    // writes and deletes it.
    defineHostModule("@luna/core", LunaCore);
    modules["@luna/lib"] = LunaLib;
    // The per-plugin copy of the lib. `src/plugins/transpile.rs` lowers a plugin's `@luna/lib`
    // import to a call on this, passing the capability that plugin's wrapper holds, so the calls
    // acting on the caller's identity travel with one. The shared object above cannot: every
    // plugin reaches it, and an identity in there would belong to whoever asked first.
    //
    // A snapshot rather than a proxy, `LunaLib`'s exports being fixed at build time. Only the
    // identity-bearing pair is rebound: `on`/`once`/`onOpenUrl` register listeners, which no
    // caller's identity decides.
    //
    // Pinned, because it is called with the CALLER's capability: a plugin that replaced this
    // factory would be handed the identity of every plugin importing `@luna/lib` after it, and
    // could act as any of them on `plugin.storage.*`, `plugin.fetch` and
    // `__Luna.registerNative`. Installed before `jsrt.load_plugins`, when no plugin has run yet.
    defineHostModule("__lunaLibFor", (cap: string) => ({
        ...LunaLib,
        ipcRenderer: {
            ...LunaLib.ipcRenderer,
            invoke: (channel: string, ...args: any[]) => invokeIpcAs(cap, channel, ...args),
            send: (channel: string, ...args: any[]) => sendIpcAs(cap, channel, ...args),
        },
    }));
    modules["@inrixia/helpers"] = InrixiaHelpers;
    modules["@luna/lib.native"] = {
        ...LibNative,
        clipboardWriteText: (text: string) => invokeIpc("__Luna.clipboardWriteText", text),
        openExternal: (url: string) => invokeIpc("__Luna.openExternal", url),
        sendToRender: (channel: string, ...args: any[]) => invokeIpc("__Luna.sendToRender", channel, ...args),
        showMessageBox: (options: any) => invokeIpc("__Luna.showMessageBox", options),
        showErrorBox: (title: string, content: string) => invokeIpc("__Luna.showErrorBox", title, content),
        showOpenDialog: (options: any) => invokeIpc("__Luna.showOpenDialog", options),
        showSaveDialog: (options: any) => invokeIpc("__Luna.showSaveDialog", options),
    };

    // Served from the Rust binary (luna_modules.rs) at /__luna__/*.mjs, not inlined. Absolute URL
    // because the injected bundle's base is about:blank; in a var for esbuild to leave it a runtime
    // import; loaded after the module registry above, which these resolve their deps through.
    try {
        const uiUrl = `${location.origin}/__luna__/ui.mjs`;
        const uiMod = await import(/* @vite-ignore */ uiUrl);
        modules["@luna/ui"] = uiMod;
        LunaPlugin.corePlugins.add("@luna/ui");
        console.log("[luna] @luna/ui core plugin loaded");
    } catch (e) {
        console.error("[luna] Failed to load @luna/ui:", e);
    }

    try {
        const devUrl = `${location.origin}/__luna__/dev.mjs`;
        const devMod = await import(/* @vite-ignore */ devUrl);
        modules["@luna/dev"] = devMod;
        LunaPlugin.corePlugins.add("@luna/dev");
        console.log("[luna] @luna/dev core plugin loaded");
    } catch (e) {
        console.error("[luna] Failed to load @luna/dev:", e);
    }

    // Load user plugins - Rust does dedup + multi-pass + reconciliation, then responds
    await invokeIpc("jsrt.load_plugins");
    // Populate CEF plugin registry from Rust PluginStore (for @luna/ui display)
    const { exposeLoaderApi } = require("./plugins/loader");
    exposeLoaderApi();
    console.log("[luna] Plugin execution delegated to Rust plugin manager");
};

setTimeout(init, 0);
