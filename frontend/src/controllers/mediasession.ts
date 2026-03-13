// Chromium only activates MediaSession (and shows thumbnail toolbar buttons on
// Windows) when web audio is playing.  Since our native player bypasses web
// audio entirely, we run a silent AudioContext to activate the integration.

import { sendIpc } from "../ipc";

let ctx: AudioContext | null = null;

function ensureAudioContext() {
    if (!ctx) {
        try {
            ctx = new AudioContext();
            const osc = ctx.createOscillator();
            const gain = ctx.createGain();
            gain.gain.value = 0;
            osc.connect(gain);
            gain.connect(ctx.destination);
            osc.start();
            sendIpc("player.dbg", "[MediaSession] AudioContext created, state=" + ctx.state);
        } catch (e) {
            sendIpc("player.dbg", "[MediaSession] AudioContext failed: " + e);
            return;
        }
    }
    if (ctx.state === "suspended") {
        ctx.resume().then(() => {
            sendIpc("player.dbg", "[MediaSession] AudioContext resumed, state=" + ctx!.state);
        }).catch((e) => {
            sendIpc("player.dbg", "[MediaSession] AudioContext resume failed: " + e);
        });
    }
}

export function updatePlaybackState(playing: boolean) {
    sendIpc("player.dbg", "[MediaSession] playbackState →", playing ? "playing" : "paused");
    if (playing) {
        ensureAudioContext();
        navigator.mediaSession.playbackState = "playing";
    } else {
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
        sendIpc("player.dbg", "[MediaSession] metadata:", item.title, "—", item.artist);
    } catch (e) {
        sendIpc("player.dbg", "[MediaSession] metadata failed: " + e);
    }
}

export function setupActionHandlers() {
    try {
        const ms = navigator.mediaSession;
        ms.setActionHandler("play", () => {
            sendIpc("player.dbg", "[MediaSession] action: play");
            (window as any).__TL_PLAY_PAUSE__?.();
        });
        ms.setActionHandler("pause", () => {
            sendIpc("player.dbg", "[MediaSession] action: pause");
            (window as any).__TL_PLAY_PAUSE__?.();
        });
        ms.setActionHandler("previoustrack", () => {
            sendIpc("player.dbg", "[MediaSession] action: previoustrack");
            (window as any).__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.();
        });
        ms.setActionHandler("nexttrack", () => {
            sendIpc("player.dbg", "[MediaSession] action: nexttrack");
            (window as any).__TIDAL_PLAYBACK_DELEGATE__?.playNext?.();
        });
        sendIpc("player.dbg", "[MediaSession] action handlers registered");
    } catch (e) {
        sendIpc("player.dbg", "[MediaSession] setupActionHandlers failed: " + e);
    }
}
