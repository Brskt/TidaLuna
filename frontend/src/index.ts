// this is basically electron's preload script
declare global {
    interface Window {
        ipc: {
            postMessage: (message: string) => void;
        };
        nativeInterface: any;
        NativePlayerComponent: any;
        __TIDAL_CALLBACKS__: any;
        __TIDAL_IPC_RESPONSE__: any;
        __TIDAL_RS_PLATFORM__: string;
    }
    var window: Window & typeof globalThis;
}

export interface AudioDevice {
    controllableVolume: boolean;
    id: string;
    name: string;
}


const sendIpc = (channel: string, ...args: any[]) => {
    window.ipc.postMessage(JSON.stringify({ channel, args }));
};

const pendingRequests = new Map<string, { resolve: (value: any) => void, reject: (reason: any) => void }>();

window.__TIDAL_IPC_RESPONSE__ = (id: string, error: any, result: any) => {
    const req = pendingRequests.get(id);
    if (req) {
        if (error) req.reject(error);
        else req.resolve(result);
        pendingRequests.delete(id);
    }
};

const invokeIpc = (channel: string, ...args: any[]) => {
    return new Promise((resolve, reject) => {
        const id = Math.random().toString(36).substring(2);
        pendingRequests.set(id, { resolve, reject });
        window.ipc.postMessage(JSON.stringify({ channel, args, id }));
    });
};
const createApplicationController = () => {
    let delegate: any = null;


    window.__TIDAL_CALLBACKS__ = window.__TIDAL_CALLBACKS__ || {};
    window.__TIDAL_CALLBACKS__.application = {
        trigger: (method: string, ...args: any[]) => {
            if (delegate && delegate[method]) {
                delegate[method](...args);
            }
        }
    };

    return {
        applyUpdate: () => sendIpc("update.action"),
        checkForUpdatesSilently: () => sendIpc("updater.check.silently"),
        getDesktopReleaseNotes: () => JSON.stringify({}),
        getPlatform: () => window.__TIDAL_RS_PLATFORM__,
        getPlatformTarget: () => "standalone",
        getProcessUptime: () => 100,
        getVersion: () => "2.38.6.6",
        getWindowsVersionNumber: () => Promise.resolve("10.0.0"),
        ready: () => sendIpc("web.loaded"),
        reenableAutoUpdater: () => sendIpc("update.reenable"),
        registerDelegate: (d: any) => { delegate = d; },
        reload: () => window.location.reload(),
        setWebVersion: (v: string) => sendIpc("web.version.set", v),
    };
};

const createAudioHack = () => {
    return {
        pause: () => { },
        play: () => { }
    }
}

const createNavigationController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => { delegate = d; },
        goBack: () => { if (delegate && delegate.goBack) delegate.goBack(); },
        goForward: () => { if (delegate && delegate.goForward) delegate.goForward(); },

    }
}

const createPlaybackController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => { delegate = d; },
        sendPlayerCommand: (cmd: any) => { },
        setCurrentMediaItem: (item: any) => {
            if (item && typeof item === "object") {
                let artist = "";
                if (item.artist) {
                    artist = typeof item.artist === "string" ? item.artist : item.artist?.name || "";
                } else if (Array.isArray(item.artists)) {
                    artist = item.artists.map((a: any) => typeof a === "string" ? a : a?.name || "").join(", ");
                }
                sendIpc("player.metadata", {
                    title: item.title || item.name || "",
                    artist,
                    quality: item.audioQuality || item.quality || "",
                });
            }
        },
        setCurrentTime: (time: any) => { },
        setPlayQueueState: (state: any) => { },
        setPlayingStatus: (status: any) => { },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}

const createUserSession = () => {
    let delegate: any = null;
    return {
        clear: () => { },
        registerDelegate: (d: any) => { delegate = d; },
        update: (s: any) => { },
    }
}

const createUserSettings = () => {
    let settings = new Map<string, any>();
    return {
        get: (key: string) => settings.get(key),
        set: (key: string, value: any) => console.log(settings.set(key, value)),
    }
}

const createWindowController = (initialState: { isMaximized: boolean, isFullscreen: boolean }) => {
    let isMaximized = initialState.isMaximized;
    let isFullscreen = initialState.isFullscreen;
    const listeners: Record<string, Function[]> = {};

    const updateState = (maximized: boolean, fullscreen: boolean) => {
        const maxChanged = isMaximized !== maximized;
        const fullChanged = isFullscreen !== fullscreen;

        isMaximized = maximized;
        isFullscreen = fullscreen;

        if (maxChanged) {
            const event = maximized ? "maximize" : "unmaximize";
            if (listeners[event]) listeners[event].forEach(cb => cb());
        }
        if (fullChanged) {
            const event = fullscreen ? "enter-full-screen" : "leave-full-screen";
            if (listeners[event]) listeners[event].forEach(cb => cb());
        }
    };

    window.__TIDAL_CALLBACKS__ = window.__TIDAL_CALLBACKS__ || {};
    window.__TIDAL_CALLBACKS__.window = {
        updateState: updateState
    };

    return {
        close: () => sendIpc("window.close"),
        isFullscreen: () => isFullscreen,
        isMaximized: () => isMaximized,
        maximize: () => sendIpc("window.maximize"),
        minimize: () => sendIpc("window.minimize"),
        on: (event: string, callback: any) => {
            if (!listeners[event]) listeners[event] = [];
            listeners[event].push(callback);
        },
        openMenu: (x: number, y: number) => sendIpc("menu.clicked", x, y),
        unmaximize: () => sendIpc("window.unmaximize"),
    }
}

const generateCodeVerifier = () => {
    const array = new Uint8Array(32);
    window.crypto.getRandomValues(array);
    return btoa(String.fromCharCode.apply(null, Array.from(array))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
};

const generateCodeChallenge = async (verifier: string) => {
    const encoder = new TextEncoder();
    const data = encoder.encode(verifier);
    const hash = await window.crypto.subtle.digest('SHA-256', data);
    const hashArray = Array.from(new Uint8Array(hash));
    return btoa(String.fromCharCode.apply(null, hashArray)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
};
const createNativePlayerComponent = () => {
    let activeEmitter: any = null;
    let activePlayer: any = null;
    let activeGen = 0;
    const Player = () => {
        const eventEmitter = {
            listeners: {} as Record<string, Function[]>,
            addListener(event: string, cb: any) {
                if (!this.listeners[event]) this.listeners[event] = [];
                this.listeners[event].push(cb);
            },
            removeListener(event: string, cb: any) {
                if (this.listeners[event]) {
                    this.listeners[event] = this.listeners[event].filter((x: any) => x !== cb);
                }
            },
            on(event: string, cb: any) {
                this.addListener(event, cb);
            },
            emit(event: string, arg: any) {
                if (this.listeners[event]) {
                    this.listeners[event].forEach(cb => cb(arg));
                }
            }
        };

        activeEmitter = eventEmitter;

        const player = {
            currentTime: 0,
            duration: 0,
            addEventListener: (event: string, cb: any) => {
                eventEmitter.addListener(event, cb);
            },
            removeEventListener: (event: string, cb: any) => eventEmitter.removeListener(event, cb),
            on: (event: string, cb: any) => eventEmitter.on(event, cb),
            disableMQADecoder: () => {

            },
            enableMQADecoder: () => {
            },
            listDevices: () => {
                sendIpc("player.devices.get");
            },
            load: (url: string, streamFormat: string, encryptionKey: string = "") => {
                player.currentTime = 0;
                activeEmitter?.emit?.("mediacurrenttime", { target: 0 });
                sendIpc("player.load", url, streamFormat, encryptionKey);
            },
            play: () => { sendIpc("player.play"); },
            pause: () => { sendIpc("player.pause"); },
            stop: () => { sendIpc("player.stop"); },
            seek: (time: number) => {
                sendIpc("player.seek", time);
            },
            setVolume: (volume: number) => {
                sendIpc("player.volume", volume);
            },
            preload: (url: string, streamFormat: string, encryptionKey: string = "") => {
                sendIpc("player.preload", url, streamFormat, encryptionKey);
            },
            cancelPreload: () => {
                sendIpc("player.preload.cancel");
            },
            recover: (url: string, encryptionKey: string = "") => {

            },
            releaseDevice: () => {
                console.log("Releasing device");
            },
            selectDevice: (device: AudioDevice, mode: "shared" | "exclusive") => {
                sendIpc("player.devices.set", device.id, mode);
            },
            selectSystemDevice: () => {
                sendIpc("player.devices.set", 'auto');
            }
        };
        activePlayer = player;
        return player;
    }

    return {
        Player,
        trigger: (event: string, target: any, gen?: number) => {
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }
            event != "mediacurrenttime" && console.debug("NativePlayer:", event, target);
            if (event === "mediacurrenttime" && activePlayer) {
                activePlayer.currentTime = target;
            } else if (event === "mediaduration" && activePlayer) {
                activePlayer.duration = target;
            }
            activeEmitter?.emit?.(event, { target });
        }
    };
}

// Synchronous initialization: expose nativeInterface immediately so Tidal
// detects desktop mode before its own scripts run.
const credentials: { credentialsStorageKey: string; codeChallenge: string; redirectUri: string; codeVerifier: string } = {
    credentialsStorageKey: "tidal",
    codeChallenge: sessionStorage.getItem("pkce_challenge") || "",
    redirectUri: "tidal://auth/",
    codeVerifier: sessionStorage.getItem("pkce_verifier") || "",
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
    window: createWindowController({ isMaximized: false, isFullscreen: false }),
};
window.NativePlayerComponent = createNativePlayerComponent();
console.log("Native Interface exposed (sync)");

// Async hydration: generate PKCE if missing, fetch real window state.
const init = async () => {
    const now = Date.now();

    if (!credentials.codeVerifier || !credentials.codeChallenge) {
        const verifier = generateCodeVerifier();
        const challenge = await generateCodeChallenge(verifier);
        credentials.codeVerifier = verifier;
        credentials.codeChallenge = challenge;
        sessionStorage.setItem("pkce_verifier", verifier);
        sessionStorage.setItem("pkce_challenge", challenge);
        console.log("Generated new PKCE pair");
    } else {
        console.log("Restored PKCE pair from session storage");
    }

    try {
        const state = await invokeIpc("window.state.get") as any;
        if (state) {
            const cb = window.__TIDAL_CALLBACKS__?.window;
            if (cb && typeof cb.updateState === "function") {
                cb.updateState(state.isMaximized, state.isFullscreen);
            }
        }
    } catch (e) {
        console.error("Failed to get window state", e);
    }

    // Frameless window management: controls + drag/resize (non-Linux only).
    // On Linux, native decorations handle this (with_decorations(true)).
    const frameless = window.__TIDAL_RS_PLATFORM__ !== "linux";

    // Window controls: minimize, maximize/restore, close (Windows 10/11 style).
    if (frameless && !document.getElementById("window-controls")) {
        const svg = (paths: string) =>
            `<svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="square">${paths}</svg>`;

        const icons = {
            minimize: svg(`<line x1="1" y1="5" x2="9" y2="5"/>`),
            maximize: svg(`<rect x="1" y="1" width="8" height="8"/>`),
            restore: svg(`<polyline points="3,1 9,1 9,7"/><rect x="1" y="3" width="6" height="6"/>`),
            close: svg(`<line x1="1" y1="1" x2="9" y2="9"/><line x1="9" y1="1" x2="1" y2="9"/>`),
        };

        const style = document.getElementById("window-controls-style") || document.createElement("style");
        style.id = "window-controls-style";
        style.textContent = `
            #window-controls { position:fixed; top:0; right:0; z-index:999999; display:flex; height:32px; background:rgba(0,0,0,0.75); border-radius:0 0 0 4px; border-bottom:1px solid rgba(255,255,255,0.08); border-left:1px solid rgba(255,255,255,0.08); -webkit-app-region:no-drag; }
            #window-controls button {
                width:46px; height:32px; border:none; background:transparent;
                color:rgba(255,255,255,0.9); cursor:default; display:flex;
                align-items:center; justify-content:center; padding:0;
                outline:none; -webkit-app-region:no-drag;
            }
            #window-controls button:hover { background:rgba(255,255,255,0.1); }
            #window-controls button:active { background:rgba(255,255,255,0.05); }
            #window-controls button:focus-visible { outline:1px solid rgba(255,255,255,0.5); outline-offset:-1px; }
            #window-controls button.close:hover { background:#e81123; color:#fff; }
            #window-controls button.close:active { background:#bf0f1d; }
        `;
        document.head.appendChild(style);

        const container = document.createElement("div");
        container.id = "window-controls";

        const makeBtn = (icon: string, cls: string, label: string, onClick: () => void) => {
            const btn = document.createElement("button");
            btn.setAttribute("type", "button");
            btn.setAttribute("aria-label", label);
            btn.setAttribute("title", label);
            btn.className = cls;
            btn.innerHTML = icon;
            btn.addEventListener("click", (e) => { e.stopPropagation(); onClick(); });
            return btn;
        };

        const minimizeBtn = makeBtn(icons.minimize, "min", "Minimize", () => sendIpc("window.minimize"));
        const maximizeBtn = makeBtn(
            windowState.isMaximized ? icons.restore : icons.maximize,
            "max",
            windowState.isMaximized ? "Restore" : "Maximize",
            () => {
                const maximized = window.nativeInterface?.window?.isMaximized?.();
                sendIpc(maximized ? "window.unmaximize" : "window.maximize");
            }
        );
        const closeBtn = makeBtn(icons.close, "close", "Close", () => sendIpc("window.close"));

        container.append(minimizeBtn, maximizeBtn, closeBtn);
        document.documentElement.appendChild(container);

        // Update maximize icon and aria-label when window state changes.
        const cb = window.__TIDAL_CALLBACKS__?.window;
        if (cb) {
            cb._maxBtn = maximizeBtn;
            cb._maxIcons = icons;
            if (!cb._controlsWrapped && typeof cb.updateState === "function") {
                const origUpdateState = cb.updateState;
                cb._controlsWrapped = true;
                cb.updateState = (maximized: boolean, fullscreen: boolean) => {
                    origUpdateState(maximized, fullscreen);
                    const btn = cb._maxBtn;
                    const ic = cb._maxIcons;
                    if (btn && ic) {
                        btn.innerHTML = maximized ? ic.restore : ic.maximize;
                        btn.setAttribute("aria-label", maximized ? "Restore" : "Maximize");
                        btn.setAttribute("title", maximized ? "Restore" : "Maximize");
                    }
                };
            }
        }
    }

    // Window resize + drag: borderless window management (non-Linux only).
    if (frameless) {
    const RESIZE_BORDER = 6;
    const RESIZE_HOT_ZONE = RESIZE_BORDER + 8;
    const INTERACTIVE = "a, button, input, select, textarea, [role='button'], img, svg";
    const FALLBACK_TITLEBAR_HEIGHT = 48;

    const hitTest = (x: number, y: number): string | null => {
        const w = window.innerWidth;
        const h = window.innerHeight;
        const top = y <= RESIZE_BORDER;
        const bottom = y >= h - RESIZE_BORDER;
        const left = x <= RESIZE_BORDER;
        const right = x >= w - RESIZE_BORDER;
        if (top && left) return "nw";
        if (top && right) return "ne";
        if (bottom && left) return "sw";
        if (bottom && right) return "se";
        if (top) return "n";
        if (bottom) return "s";
        if (left) return "w";
        if (right) return "e";
        return null;
    };

    const isNearResizeEdge = (x: number, y: number): boolean => {
        const w = window.innerWidth;
        const h = window.innerHeight;
        return (
            x <= RESIZE_HOT_ZONE ||
            x >= w - RESIZE_HOT_ZONE ||
            y <= RESIZE_HOT_ZONE ||
            y >= h - RESIZE_HOT_ZONE
        );
    };

    const getTitlebarHeight = (): number => {
        const candidates = document.querySelectorAll("header, [role='banner']");
        for (const el of candidates) {
            const rect = el.getBoundingClientRect();
            if (rect.top <= 0 && rect.height > 0 && rect.height < 120) {
                return rect.bottom;
            }
        }
        return FALLBACK_TITLEBAR_HEIGHT;
    };

    // Resize cursor: only send IPC when direction changes.
    let lastDir: string | null = null;
    let mouseFramePending = false;
    let pendingMouseX = 0;
    let pendingMouseY = 0;
    let pendingMouseTarget: HTMLElement | null = null;
    const resetResizeCursor = () => {
        if (lastDir !== null) {
            lastDir = null;
            sendIpc("window.cursor.reset");
        }
    };
    const processResizeMouse = () => {
        mouseFramePending = false;
        if (window.nativeInterface?.window?.isMaximized?.()) {
            resetResizeCursor();
            return;
        }
        if (pendingMouseTarget && pendingMouseTarget.closest(INTERACTIVE)) {
            resetResizeCursor();
            return;
        }
        if (!isNearResizeEdge(pendingMouseX, pendingMouseY)) {
            resetResizeCursor();
            return;
        }
        const dir = hitTest(pendingMouseX, pendingMouseY);
        if (dir !== lastDir) {
            lastDir = dir;
            if (dir) {
                sendIpc("window.cursor", dir);
            } else {
                sendIpc("window.cursor.reset");
            }
        }
    };
    document.addEventListener("mousemove", (e) => {
        pendingMouseX = e.clientX;
        pendingMouseY = e.clientY;
        pendingMouseTarget = e.target as HTMLElement | null;
        if (!mouseFramePending) {
            mouseFramePending = true;
            requestAnimationFrame(processResizeMouse);
        }
    }, true);

    document.addEventListener("mouseleave", () => {
        resetResizeCursor();
    }, true);

    // Mousedown: resize (border) > drag (titlebar) > passthrough.
    document.addEventListener("mousedown", (e) => {
        if (e.button !== 0) return;
        const maximized = window.nativeInterface?.window?.isMaximized?.();

        // Resize takes priority (skip if maximized).
        if (!maximized) {
            if (isNearResizeEdge(e.clientX, e.clientY)) {
                const dir = hitTest(e.clientX, e.clientY);
                if (dir) {
                    e.preventDefault();
                    sendIpc("window.resize", dir);
                    return;
                }
            }
        }

        // Titlebar drag.
        if (e.clientY > getTitlebarHeight()) return;
        const target = e.target as HTMLElement | null;
        if (!target || target.closest(INTERACTIVE)) return;
        e.preventDefault();
        if (e.detail === 2) {
            sendIpc(maximized ? "window.unmaximize" : "window.maximize");
        } else {
            sendIpc("window.drag");
        }
    }, true);
    } // end frameless

    // F12 devtools fallback (in case the WebView captures the key before tao).
    document.addEventListener("keydown", (e) => {
        if (e.key === "F12") {
            e.preventDefault();
            sendIpc("window.devtools");
        }
    }, true);

    console.log("Native Interface initialized in", Date.now() - now, "ms");
};

setTimeout(init, 0);
