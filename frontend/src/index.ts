// this is basically electron's preload script
import { createApplicationController } from "./controllers/application";
import { createAudioHack } from "./controllers/audio";
import { createNavigationController } from "./controllers/navigation";
import { createPlaybackController } from "./controllers/playback";
import { createUserSession } from "./controllers/session";
import { createUserSettings } from "./controllers/settings";
import { createWindowController } from "./controllers/window";
import { createNativePlayerComponent } from "./controllers/player";
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
window.__TIDAL_RS_PLAYER_PUSH__ = (events: any[]) => {
    if (!Array.isArray(events)) return;
    const bridge = window.NativePlayerComponent;
    if (!bridge || typeof bridge.trigger !== "function") return;
    for (const event of events) {
        if (!event || typeof event !== "object") continue;
        const type = event.t;
        if (type === "time") {
            bridge.trigger("mediacurrenttime", event.v, event.seq);
        } else if (type === "duration") {
            bridge.trigger("mediaduration", event.v, event.seq);
        } else if (type === "state") {
            bridge.trigger("mediastate", event.v, event.seq);
        } else if (type === "devices") {
            bridge.trigger("devices", event.v);
        }
    }
};
console.log("Native Interface exposed (sync)");

// Async hydration.
const init = async () => {
    const now = Date.now();
    initWindowControls();
    console.log("Native Interface initialized in", Date.now() - now, "ms");
};

setTimeout(init, 0);
