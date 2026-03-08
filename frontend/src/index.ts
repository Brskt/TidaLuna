// this is basically electron's preload script
import { createApplicationController } from "./controllers/application";
import { createAudioHack } from "./controllers/audio";
import { createNavigationController } from "./controllers/navigation";
import { createPlaybackController } from "./controllers/playback";
import { createUserSession } from "./controllers/session";
import { createUserSettings } from "./controllers/settings";
import { createWindowController } from "./controllers/window";
import { createNativePlayerComponent } from "./controllers/player";
import { sendIpc } from "./ipc";
import { initWindowControls } from "./ui/window-controls";

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
window.__TIDAL_RS_PLAYER_PUSH__ = (events: any[]) => {
    if (!Array.isArray(events)) return;
    const bridge = window.NativePlayerComponent;
    if (!bridge || typeof bridge.trigger !== "function") return;
    for (const event of events) {
        if (!event || typeof event !== "object") continue;
        const type = event.t;
        const mapped = SEQ_EVENTS[type];
        if (mapped) {
            bridge.trigger(mapped, event.v, event.seq);
        } else if (PASSTHROUGH_EVENTS.has(type)) {
            bridge.trigger(type, event.v);
        }
    }
};
// --- Audio constructor interception for debugging ---
(() => {
    // Intercept Audio constructor
    const OrigAudio = window.Audio;
    (window as any).Audio = function(src?: string) {
        sendIpc("player.dbg", "new Audio()", src || "no-src");
        const el = new OrigAudio(src);
        return el;
    };
    (window as any).Audio.prototype = OrigAudio.prototype;
})();

console.log("Native Interface exposed (sync)");

// Find the TIDAL web app's Redux store via React fiber internals.
const findReduxStore = () => {
    for (const el of document.querySelectorAll("*")) {
        const fiberKey = Object.keys(el).find(k => k.startsWith("__reactFiber"));
        if (!fiberKey) continue;
        let fiber = (el as any)[fiberKey];
        while (fiber) {
            if (fiber.memoizedProps?.store?.dispatch) {
                return fiber.memoizedProps.store;
            }
            fiber = fiber.return;
        }
    }
    return null;
};

// Async hydration.
const init = async () => {
    const now = Date.now();
    initWindowControls();

    // Capture Redux store once the app has rendered (max 30s).
    let attempts = 0;
    const poll = setInterval(() => {
        const store = findReduxStore();
        if (store) {
            (window as any).__TL_REDUX_STORE__ = store;
            clearInterval(poll);
        } else if (++attempts >= 60) {
            console.warn("Redux store not found after 30s, giving up");
            clearInterval(poll);
        }
    }, 500);

    console.log("Native Interface initialized in", Date.now() - now, "ms");
};

setTimeout(init, 0);
