// Chromium only activates MediaSession (and shows thumbnail toolbar buttons on
// Windows) when web audio is playing.  Since our native player bypasses web
// audio entirely, we run a silent AudioContext to activate the integration.

import { sendDbgIpc } from "../ipc";

// pointerdown covers mouse/touch/pen, keydown covers keyboard-only users.
const GESTURE_EVENTS = ["pointerdown", "keydown", "touchstart"] as const;

let ctx: AudioContext | null = null;
let gestureArmed = false;
// True while playback is active; gates the gesture fallback.
let wantRunning = false;
let onGesture: (() => void) | null = null;

function disarmGestureResume() {
    if (!gestureArmed) return;
    gestureArmed = false;
    if (onGesture) {
        for (const ev of GESTURE_EVENTS) window.removeEventListener(ev, onGesture, true);
        onGesture = null;
    }
}

function createContext(): AudioContext | null {
    try {
        // Route the silent context to a sink with no hardware device so it never
        // contends with the native exclusive-WASAPI output (exclusive mode
        // invalidates any shared client on it, which throws an AudioContext error).
        // The graph still renders, so MediaSession stays active. Fall back if unsupported.
        let c: AudioContext;
        try {
            const opts: AudioContextOptions & { sinkId?: { type: string } } = {
                sinkId: { type: "none" },
            };
            c = new AudioContext(opts);
        } catch {
            c = new AudioContext();
        }
        const osc = c.createOscillator();
        const gain = c.createGain();
        gain.gain.value = 0;
        osc.connect(gain);
        gain.connect(c.destination);
        osc.start();
        // Closed is terminal (no resume, no new nodes): drop it so the next
        // ensureAudioContext() rebuilds. resume()'s transition to "running" is
        // async, so statechange is the reliable de-arm signal, not a sync read.
        c.addEventListener("statechange", () => {
            if (ctx !== c) return;
            if (c.state === "closed") ctx = null;
            else if (c.state === "running") disarmGestureResume();
        });
        sendDbgIpc("[MediaSession] AudioContext created, state=" + c.state);
        return c;
    } catch (e) {
        sendDbgIpc("[MediaSession] AudioContext failed: " + e);
        return null;
    }
}

// resume() only takes effect under a user gesture; retry on the next
// interaction (de-armed once the context reaches running).
function armGestureResume() {
    if (gestureArmed) return;
    gestureArmed = true;
    onGesture = () => {
        // Never resume the silent context while playback is paused.
        if (!wantRunning) return;
        ctx?.resume().then(() => {
            if (ctx?.state === "running") disarmGestureResume();
        }).catch(() => {});
    };
    for (const ev of GESTURE_EVENTS) window.addEventListener(ev, onGesture, { capture: true });
}

function ensureAudioContext() {
    // Recreate when missing or terminally closed.
    if (!ctx || ctx.state === "closed") {
        ctx = createContext();
        if (!ctx) return;
    }
    if (ctx.state === "suspended") {
        ctx.resume().then(() => {
            // resume() resolves even when the autoplay policy keeps the context
            // suspended, so check the real state instead of trusting the promise.
            if (ctx?.state === "running") {
                sendDbgIpc("[MediaSession] AudioContext running");
            } else {
                armGestureResume();
            }
        }).catch((e) => {
            sendDbgIpc("[MediaSession] AudioContext resume failed: " + e);
            armGestureResume();
        });
    }
}

export function updatePlaybackState(playing: boolean) {
    sendDbgIpc("[MediaSession] playbackState →", playing ? "playing" : "paused");
    if (playing) {
        wantRunning = true;
        ensureAudioContext();
        navigator.mediaSession.playbackState = "playing";
    } else {
        wantRunning = false;
        // Pausing: drop the armed gesture fallback too.
        disarmGestureResume();
        ctx?.suspend().catch((e) => console.warn("[luna:mediasession] AudioContext suspend failed:", e));
        navigator.mediaSession.playbackState = "paused";
    }
}

export function updateMetadata(item: any) {
    if (!item || typeof item !== "object") return;
    const artwork = item.imageUrl
        ? [{ src: item.imageUrl, sizes: "512x512", type: "image/jpeg" }]
        : [];
    try {
        navigator.mediaSession.metadata = new MediaMetadata({
            title: item.title || "",
            artist: item.artist || "",
            album: item.album || "",
            artwork,
        });
        sendDbgIpc("[MediaSession] metadata:", item.title, "-", item.artist);
    } catch (e) {
        sendDbgIpc("[MediaSession] metadata failed: " + e);
    }
}

export function setupActionHandlers() {
    try {
        const ms = navigator.mediaSession;
        ms.setActionHandler("play", () => {
            sendDbgIpc("[MediaSession] action: play");
            (window as any).__TL_PLAY_PAUSE__?.();
        });
        ms.setActionHandler("pause", () => {
            sendDbgIpc("[MediaSession] action: pause");
            (window as any).__TL_PLAY_PAUSE__?.();
        });
        ms.setActionHandler("previoustrack", () => {
            sendDbgIpc("[MediaSession] action: previoustrack");
            (window as any).__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.();
        });
        ms.setActionHandler("nexttrack", () => {
            sendDbgIpc("[MediaSession] action: nexttrack");
            (window as any).__TIDAL_PLAYBACK_DELEGATE__?.playNext?.();
        });
        sendDbgIpc("[MediaSession] action handlers registered");
    } catch (e) {
        sendDbgIpc("[MediaSession] setupActionHandlers failed: " + e);
    }
}
