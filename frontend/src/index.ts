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
import { createNativePlayerComponent, intentOfTrackList, moveOf, pickOf, PRELOAD_ITEM, unloadBeforeNext } from "./controllers/player";
import { sdkPlayer, startSdkPlayer } from "./sdk-player";
import type { LoadRefusal, TrackListPayload } from "./controllers/player";
import { followCastQueue } from "./connect/middleware";
import { updatePlaybackState } from "./controllers/mediasession";
import { proxySetPlaying, proxySetTime, proxySetDuration, proxyReset, proxyFail, isSelfLoad } from "./audio-proxy";
import { askRefusedAgain, playOnRefusal, refusalDelivered, refusalStands, refusePlayback } from "./refuse-playback";
import { installSdkFetchObserver } from "./sdk-playbackinfo";
import { isStoreNotPublished } from "../plugins/lib/src/redux/storeNotPublished";
import { followCommittedVolume, isOutputLevelReport, mirrorRustLevel, resyncOutputLevel } from "./volume";
import { followCommittedOutput } from "./output-device";
import { initWindowControls } from "./ui/window-controls";
import { invokeIpc, invokeIpcAs, sendIpc, sendIpcAs, isLoginCallback, onIpcEvent } from "./ipc";
import { initPerfOverlay } from "./debug/perf-overlay";

// @luna/core and @luna/lib - safe to import after bootstrap
import { defineHostModule, initCore, modules, LunaPlugin } from "../render/src";
import * as LunaCore from "../render/src";
import * as LunaLib from "../plugins/lib/src";
import * as InrixiaHelpers from "@inrixia/helpers";
import * as LibNative from "../plugins/lib.native/src/index.native";

// Top level: it runs before TIDAL's SDK asks for its first playbackinfo and before any plugin
// loads (`jsrt.load_plugins` is awaited in `init`, below). A plugin that took the slot first would
// see every answer the page's `fetch` returns.
installSdkFetchObserver();

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

// Binds the TIDAL Connect relays at bundle load. Nothing here waits for the store (the lib's lazy
// proxy); the state snapshot inside does, and reports its own failure.
try {
    const { store } = require("../plugins/lib/src/redux/store");
    setupConnectEventListeners(store);
} catch {
    // Only a store module that fails to load lands here, and nothing binds the relays afterwards.
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
    "deviceasiodrivernotfound", "deviceasioformatunsupported",
    "deviceasioinitfailed", "deviceasiorateunsupported",
    "deviceexclusiveformatunsupported",
    "deviceasiobackenddied", "deviceexclusivebackenddied",
]);
// Names Rust emits that this bridge drops on purpose. Listing them is what lets the arm at the
// end of the dispatch treat every other unknown name as drift rather than as noise.
const IGNORED_EVENTS = new Set(["volume"]);
// Monotonic, like the stamp it is compared with: a wall clock stepped back would hold the
// throttle shut for the whole step. -Infinity because `performance.now()` is near 0 at start.
let _lastTimeDispatch = -Infinity;
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
    // One event's handling, apart from its neighbours': a consumer that throws (the SDK's listeners
    // run inside `bridge.trigger`) must not cost the events behind it, whose `state` or `duration`
    // the SDK waits on.
    const handle = (event: any): void => {
        if (!event || typeof event !== "object") return;
        const type = event.t;
        if (type === "mediaformat") {
            (window as any).__LUNAR_MEDIA_FORMAT__ = event.v;
            for (const r of _mediaFormatResolvers) r(event.v);
            _mediaFormatResolvers = [];
        }
        if (type === "time") {
            proxySetTime(event.v);
            const now = performance.now();
            if (_forceTimeDispatch || now - _lastTimeDispatch >= 250) {
                _forceTimeDispatch = false;
                _lastTimeDispatch = now;
                try {
                    const { store } = require("../plugins/lib/src/redux/store");
                    store.dispatch({ type: "playbackControls/TIME_UPDATE", payload: event.v });
                } catch (_) {}
            }
        } else if (type === "duration") {
            const product = typeof event.product === "string" ? event.product : undefined;
            // Rust announces this for every load, even one with no length (src/player/thread/commands.rs):
            // it is how the page learns which track Rust holds. States carry no track; `PlayState`
            // reads it when `active` arrives, to know whose time the segment it opens is.
            (window as any).__LUNAR_RUST_PRODUCT_ID__ = product;
            proxySetDuration(event.v, product);
            if (event.v > 0) refusalDelivered(product);
        } else if (type === "volume") {
            // The OS mixer moved the level: mirrored as a controller's move is. A store not yet
            // there misses it, and the page catches up with it once the store is found.
            try {
                const { store } = require("../plugins/lib/src/redux/store");
                if (isOutputLevelReport(event.v)) mirrorRustLevel(store, event.v);
            } catch (e) {
                if (!isStoreNotPublished(e)) throw e;
            }
        } else if (type === "state") {
            const playing = event.v === "active";
            // A cast's tracks are named in Connect's space, which is never compared with this page's:
            // under a cast no track Rust holds can be named here.
            if (event.cast) (window as any).__LUNAR_RUST_PRODUCT_ID__ = undefined;
            (window as any).__TL_PLAYING__ = playing;
            updatePlaybackState(playing);
            if (event.v === "completed") {
                proxyReset();
                // `cast`: a TIDAL Connect controller owns the player, so the track that ended
                // is one this queue never chose, and the controller has its next already.
                // `_selfLoad` cannot say so: it names the last load THIS page issued.
                if (isSelfLoad() && !event.cast) {
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
        } else if (type === "mediacastowned") {
            // Rust refused a load: a TIDAL Connect controller owns the player. `load` names the
            // SDK load refused (none for a self-load, which the SDK does not wait on). The SDK
            // waits with no timeout, and 0 is its unknown-duration sentinel. No seq: the refusal
            // minted no generation, and the cast's events must keep outranking it.
            const sdkLoad = event.v?.load !== undefined;
            // Before the duration that settles the load: the SDK's autoplay runs off that await,
            // and its play is for the refused track, which would resume the cast's instead.
            const refusal: LoadRefusal | undefined = sdkLoad ? bridge.refuseLoad?.(event.v.load) : undefined;
            if (!sdkLoad || refusal?.kind === "pick") {
                refusePlayback(
                    "tidalunar_cast_owns_player",
                    "a TIDAL Connect controller owns the player",
                    "App is being controlled via another client. Disconnect to control the app again.",
                );
                if (sdkLoad) bridge.trigger("mediaduration", 0);
            } else if (refusal?.kind === "automatic") {
                // Nobody asked, nobody is told; the transport stays the cast's.
                bridge.settleLoad?.(0);
                try {
                    const { store } = require("../plugins/lib/src/redux/store");
                    followCastQueue(store, refusal.productId);
                } catch (_) {}
            } else if (refusal?.kind === "moved") {
                // The cast answers the listener's step as the controller's own: nothing to tell,
                // and the SDK's play of its reload stays off the cast.
                bridge.settleLoad?.(refusal.duration);
            }
        } else if (type === "mediacastrouted") {
            // A load the TIDAL Connect controller's queue plays: a listener's pick, as the
            // controller's own pick would be, or the track the cast already plays. Settled as one
            // that ran, with the catalog's length (`routeLoad`; `moved` in LoadRefusal says why).
            const duration = bridge.routeLoad?.(event.v?.load);
            if (typeof duration === "number") bridge.settleLoad?.(duration);
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
                // exclusive once the device frees. TIDAL saves the mode it commits with the
                // device, which is what a restart restores.
                try {
                    const { store } = require("../plugins/lib/src/redux/store");
                    store.dispatch({ type: "player/SET_DEVICE_MODE", payload: "shared" });
                } catch (_) {}
            }
            if (
                type === "deviceasiodrivernotfound" ||
                type === "deviceasioformatunsupported" ||
                type === "deviceasioinitfailed" ||
                // A dead control thread is the strongest case this branch exists for: whatever
                // killed it is in the driver or the path reaching it, so a restart that re-seeds
                // ASIO walks straight back into it. The exclusive twin is deliberately absent
                // above, its own clear reading "the device cannot do exclusive", which a panic
                // says nothing about.
                type === "deviceasiobackenddied"
            ) {
                // ASIO failed; Rust fell back to shared output. Clear the flag AND persist it,
                // keeping a restart from re-seeding ASIO and re-entering the failing path.
                (window as any).__TIDALUNAR_ASIO__ = false;
                sendIpc("settings.asio", false);
            }
        } else if (!IGNORED_EVENTS.has(type)) {
            // A name Rust emits that nothing above claims is dropped without a trace, which is
            // exactly how two backend-death events reached no listener at all. The updater's
            // dispatch learned the same lesson and keeps a warn of its own for it.
            console.warn(`[bridge] unhandled player event: ${type}`);
        }
    };
    for (const event of events) {
        try {
            handle(event);
        } catch (e) {
            console.error("[bridge] player event failed:", event?.t, e);
        }
    }
};
console.log("Native Interface exposed (sync)");

// Fetch proxy, XHR patch, and token capture live in early_runtime.js (on_context_created).

const init = async () => {
    const now = performance.now();
    initWindowControls();
    console.log("Native Interface initialized in", performance.now() - now, "ms");

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
    const { store } = require("../plugins/lib/src/redux/store");

    // The level this install plays, which another account's page, a controller or the OS mixer
    // may have set while no store could mirror it.
    resyncOutputLevel(store).catch((e) =>
        console.error("[luna] Output level resync failed:", e),
    );

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
        const { interceptors, putInterceptors } = require("../render/src/exposeTidalInternals.patchAction");
        const add = (action: string, cb: Function, registry: Record<string, Set<Function>> = interceptors) => {
            registry[action] ??= new Set();
            registry[action].add(cb);
        };
        add("playbackControls/PLAY", () => {
            // A track this page refused asks for the load again, and Rust's own track stays as it is.
            // The Play is cancelled: it goes out again once the track is loaded.
            switch (playOnRefusal()) {
                case "retry":
                case "wait":
                    return true;
                case "resume":
                    break;
            }
            // Self-load (DASH/AAC) keeps its direct path; SDK tracks go through the
            // deduped desired-intent: a user PLAY reaches Rust even when TIDAL
            // resumes via stop+load(same) without calling nativePlayer.play().
            if (isSelfLoad()) sendIpc("player.play");
            else (window.NativePlayerComponent as any)?.setDesiredPlayback?.(true);
        });
        add("playbackControls/PAUSE", () => {
            if (!isSelfLoad()) (window.NativePlayerComponent as any)?.setDesiredPlayback?.(false);
        });
        // TIDAL puts this at the start of every load it begins (its pick, its Play on a stopped transport),
        // naming the product, and the SDK's own retries of a request put nothing: the one signal that the
        // listener asked for the product again. A product this page refused is then asked for once more.
        add(
            "playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION",
            (p: { mediaProduct?: { productId?: unknown } } | undefined) => {
                if (askRefusedAgain(String(p?.mediaProduct?.productId ?? ""))) window.nativeInterface.playback.replaySelfLoad();
            },
            putInterceptors,
        );
        // TIDAL stops by resetting the SDK's players. The native player's reset() sends Rust a stop
        // only for a session it loaded itself, and Shaka's only unloads the element. A track this
        // page loaded (Rust's, behind an element that mirrors it) kept playing under a stopped
        // transport. Both rails: a dispatched STOP (this page's refusals) and the one TIDAL's saga
        // puts after a queue clear. Rust drops the stop while a cast owns the player.
        const stopSelfLoad = () => {
            if (isSelfLoad()) sendIpc("player.stop");
        };
        add("playbackControls/STOP", stopSelfLoad);
        add("playbackControls/STOP", stopSelfLoad, putInterceptors);
        // TIDAL's "play this" signal fires BEFORE the selected track's load (async
        // getPlaybackInfo gap) while the OLD track is still committed; resuming here audibly
        // replays the OLD track (the "bleed"). Request auto-play on the SELECTED track's
        // load instead of a separate play: fixes the bleed and the startup first-play.
        add("playQueue/ADD_TRACK_LIST_TO_PLAY_QUEUE", (p: TrackListPayload) => {
            const bridge = window.NativePlayerComponent as any;
            if (intentOfTrackList(p)) bridge?.requestPlayOnLoad?.();
            const pick = pickOf(p);
            if (pick) bridge?.actOnQueue?.(pick);
        });
        // A move reaches either rail: TIDAL's saga puts it for the player's Next and Previous, and
        // this page's own advance dispatches it. Read before the queue moves.
        for (const [action, act] of [["playQueue/MOVE_NEXT", "next"], ["playQueue/MOVE_PREVIOUS", "previous"]] as const) {
            const onMove = () => {
                try {
                    const { store } = require("../plugins/lib/src/redux/store");
                    const move = moveOf(act, store.getState()?.playQueue);
                    if (move) (window.NativePlayerComponent as any)?.actOnQueue?.(move);
                } catch (_) {}
            };
            add(action, onMove);
            add(action, onMove, putInterceptors);
        }
        // TIDAL's `setNext`. A next the SDK serves from Shaka (or a video) leaves the track its native player
        // staged in Rust, which plays when the current one ends. The SDK's own unset, called ahead of
        // the new next (put again once it has settled), makes it unload that preload (its `cancelPreload`
        // reaches Rust) before it acts on the new next, with no `Bat` saga whose PRELOAD_SUCCESS would
        // release `Wat`'s fallback. Both rails: TIDAL's saga puts it for the queue's next, a component
        // dispatches it for the row under the pointer.
        const onPreloadItem = unloadBeforeNext(
            () => window.NativePlayerComponent as any,
            sdkPlayer,
            () => require("../plugins/lib/src/redux/store").store.__lunaRawDispatch,
        );
        void startSdkPlayer();
        add(PRELOAD_ITEM, onPreloadItem);
        add(PRELOAD_ITEM, onPreloadItem, putInterceptors);
        add("playbackControls/SEEK", (time: number) => {
            // The bar names the refused track, and Rust's is the one the refusal left behind.
            if (refusalStands()) return;
            sendIpc("player.seek", time);
            _forceTimeDispatch = true;
        });
        add("settings/TOGGLE_CLOSE_TO_TRAY", () => {
            const { store } = require("../plugins/lib/src/redux/store");
            const current = store.getState().settings?.desktop?.closeToTray ?? false;
            sendIpc("settings.close_to_tray", !current);
        });
    }

    // A device TIDAL switches to while shaka is its active player never reaches the delegate.
    followCommittedOutput(store);
    // A volume set here while TIDAL's player holds no stream (nothing played yet, the queue ended).
    followCommittedVolume(store);

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
