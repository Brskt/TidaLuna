import { sendIpc, sendDbgIpc } from "../ipc";
import { proxyFail, setSelfLoad } from "../audio-proxy";
import { setupActionHandlers, updateMetadata, updatePlaybackState } from "./mediasession";

// `player.parse_dash` failure code for "no decoder here handles this codec", mirroring HTTP
// 415. Set in `src/ipc/plugin/mod.rs`; every other parse failure answers 400.
const UNDECODABLE_PROFILE = 415;

export const createPlaybackController = () => {
    let delegate: any = null;
    let lastTransitionId = "";
    return {
        registerDelegate: (d: any) => {
            delegate = d;
            // Expose delegate and play/pause toggle for native menu controls.
            (window as any).__TIDAL_PLAYBACK_DELEGATE__ = d;
            (window as any).__TL_PLAY_PAUSE__ = () => {
                (window as any).__TL_PLAYING__ ? d?.pause?.() : d?.resume?.();
            };
            setupActionHandlers();
            sendDbgIpc("registerDelegate", Object.keys(d || {}).join(","));
        },
        sendPlayerCommand: (cmd: any) => {
            sendDbgIpc("sendPlayerCommand", JSON.stringify(cmd));
            if (cmd && typeof cmd === "object") {
                const type = cmd.type || cmd.command;
                if (type === "play") sendIpc("player.play");
                else if (type === "pause") sendIpc("player.pause");
                else if (type === "stop") sendIpc("player.stop");
            }
        },
        setCurrentMediaItem: (item: any) => {
            if (item && typeof item === "object") {
                // Send raw media item; metadata parsing/fallbacks are centralized in Rust.
                sendIpc("player.metadata", item);
                updateMetadata(item);

                // Fire MEDIA_PRODUCT_TRANSITION interceptors directly (not via store.dispatch
                // which triggers TIDAL's middleware and breaks the playback state machine).
                const productId = String(item.productId ?? item.id ?? "");
                if (productId && productId !== lastTransitionId) {
                    lastTransitionId = productId;
                    setTimeout(async () => {
                        try {
                            if (lastTransitionId !== productId) return;

                            await require("../../render/src/modules").storeReady;
                            if (lastTransitionId !== productId) return;

                            const { interceptors } = require("../../render/src/exposeTidalInternals.patchAction");
                            const { store } = require("../../plugins/lib/src/redux/store");
                            const actionType = "playbackControls/MEDIA_PRODUCT_TRANSITION";

                            const controls = store.getState().playbackControls;
                            if (!controls) return;

                            // Get actual audio quality from playbackInfo API (same as TIDAL's SDK)
                            let actualQuality: string | undefined;
                            let pbi: any;
                            try {
                                const { getPlaybackInfo } = require("../../plugins/lib/src/helpers/getPlaybackInfo");
                                const state = store.getState();
                                const streamingQuality = state.settings?.quality?.streaming;

                                pbi = await getPlaybackInfo(productId, streamingQuality);
                                if (!pbi && streamingQuality) {
                                    const trackItem = state.content?.mediaItems?.[productId]?.item ?? state.content?.tracks?.[productId];
                                    const fallbackQuality = trackItem?.audioQuality;
                                    if (fallbackQuality && fallbackQuality !== streamingQuality) {
                                        pbi = await getPlaybackInfo(productId, fallbackQuality);
                                    }
                                }
                                if (lastTransitionId !== productId) return; // stale after async
                                actualQuality = pbi?.audioQuality;

                                // Self-load non-FLAC BTS streams (TIDAL's boombox doesn't work in CEF)
                                if (pbi?.manifestMimeType === "application/dash+xml" && pbi.manifest) {
                                    const manifest = pbi.manifest as any;
                                    if (manifest.initUrl && manifest.segmentUrls?.length > 0) {
                                        const codec = manifest.codec?.split(".")?.[0] ?? "aac";
                                        setSelfLoad(true);
                                        // Self-load bypasses the SDK load delegate; drop any pending
                                        // select-play intent, keeping it out of a later FLAC load.
                                        (window.NativePlayerComponent as any)?.clearPlayOnLoad?.();
                                        sendIpc("player.load_dash", manifest.initUrl, JSON.stringify(manifest.segmentUrls), codec, productId);
                                    }
                                }
                                if (pbi?.manifestMimeType === "application/vnd.tidal.bts" && pbi.manifest) {
                                    const manifest = pbi.manifest;
                                    const streamUrl = manifest.urls?.[0];
                                    const mimeType: string = manifest.mimeType ?? "";
                                    if (streamUrl && !mimeType.includes("flac")) {
                                        const codec = manifest.codecs?.split(".")?.[0] ?? "aac";
                                        const encKey = manifest.keyId ?? "";
                                        setSelfLoad(true);
                                        // Self-load bypasses the SDK load delegate; drop any pending
                                        // select-play intent, keeping it out of a later FLAC load.
                                        (window.NativePlayerComponent as any)?.clearPlayOnLoad?.();
                                        // restart/wantPlay repeat the defaults this call has
                                        // always been parsed with; they are spelled out only
                                        // so productId lands on the index player.load reads.
                                        sendIpc("player.load", streamUrl, codec, encKey, false, false, productId);
                                    }
                                }
                            } catch (e) {
                                console.error("[luna:playback] playbackInfo/self-load error:", e);
                                // A rejection is a resumption too, and it reached here by
                                // skipping the check above. Returning covers the tail as well,
                                // which would otherwise write this track's identity over the
                                // current one. The log stays: it names a real failure either way.
                                if (lastTransitionId !== productId) return;
                                // 415 is the one failure a listener can act on: no decoder for
                                // this codec. Rust names it for the console, the banner below
                                // carries the human sentence. Other failures stay silent as
                                // before; a network hiccup is not a quality problem.
                                if ((e as any)?.code === UNDECODABLE_PROFILE) {
                                    // Deliberately no `player.pause`. Answering "paused" to a
                                    // TIDAL that just asked to play reads as a failed start,
                                    // and it advances the queue; measured on two tracks.
                                    proxyFail();
                                    // `mediaerror` is the sanctioned channel and this component
                                    // already speaks it: the SDK listens on us and reads
                                    // `e.target.errorCode`. It raises `player/ERROR`, winning
                                    // the one-second race TIDAL arms on every autoplay load.
                                    // Losing that race writes SET_PLAYBACK_STATE("STALLED"),
                                    // whose reducer forces the desired state back to PLAYING.
                                    // The code sits outside TIDAL's three-key map on purpose;
                                    // a mapped one raises a second banner beside ours.
                                    (window.NativePlayerComponent as any)?.trigger?.("mediaerror", {
                                        error: "stream codec not supported by this build",
                                        errorCode: "tidalunar_undecodable_profile",
                                    });
                                    // TIDAL's own pairing for a failure on the current track.
                                    // STOP is grouped with PAUSE in the reducer, never with
                                    // SKIP_NEXT; IDLE clears a STALLED without touching intent.
                                    store.dispatch({ type: "playbackControls/STOP" });
                                    store.dispatch({
                                        type: "playbackControls/SET_PLAYBACK_STATE",
                                        payload: "IDLE",
                                    });
                                    store.dispatch({
                                        type: "message/MESSAGE_ERROR",
                                        payload: {
                                            id: Date.now(),
                                            category: "PLAYBACK",
                                            severity: "ERROR",
                                            message: "TidaLunar cannot play music below 320 kbps for the moment. Please change the quality in TIDAL's settings.",
                                        },
                                    });
                                }
                            }

                            const oldCtx = controls.playbackContext ?? {};
                            const ctx = { ...oldCtx, actualProductId: productId, actualAudioQuality: actualQuality ?? oldCtx.actualAudioQuality, actualDuration: item.duration ?? oldCtx.actualDuration ?? 0, actualVideoQuality: null, bitDepth: pbi?.bitDepth ?? oldCtx.bitDepth ?? null, sampleRate: pbi?.sampleRate ?? oldCtx.sampleRate ?? null };
                            const oldMp = controls.mediaProduct ?? {};
                            const mp = { ...oldMp, productId, productType: item.type ?? "track" };

                            (window as any).__LUNAR_CURRENT_PRODUCT_ID__ = productId;

                            // Dispatch UPDATE_PLAYBACK_CONTEXT - Redux state is frozen, direct mutation fails
                            try {
                                const { buildActions } = require("../../render/src/exposeTidalInternals.patchAction");
                                const buildAction = buildActions["playbackControls/UPDATE_PLAYBACK_CONTEXT"];
                                if (buildAction) {
                                    store.dispatch(buildAction(ctx));
                                } else {
                                    store.dispatch({ type: "playbackControls/UPDATE_PLAYBACK_CONTEXT", payload: ctx });
                                }
                            } catch (e) { console.warn("[luna:playback] UPDATE_PLAYBACK_CONTEXT dispatch failed:", e); }

                            (window as any).__LUNAR_RESET_MEDIA_FORMAT__?.();

                            const cbs = interceptors[actionType];
                            if (!cbs?.size) return;
                            const payload = { mediaProduct: mp, playbackContext: ctx };
                            for (const cb of cbs) {
                                try {
                                    cb(payload, actionType);
                                } catch (e) {
                                    console.error(`[luna:playback] Interceptor error for ${actionType}:`, e);
                                }
                            }
                        } catch (e) {
                            console.warn("[luna:playback] Media transition notification failed:", e);
                        }
                    }, 0);
                }
            }
        },
        setCurrentTime: () => {
            // Absorbed. Rust owns the position; TIDAL calls this with reset values a second
            // writer would leak into the published getter.
        },
        setPlayQueueState: (state: any) => {
            sendDbgIpc("setPlayQueueState", JSON.stringify(state));
        },
        setPlayingStatus: (status: any) => {
            sendDbgIpc("setPlayingStatus", JSON.stringify(status));
            (window as any).__TL_PLAYING__ = !!status;
            updatePlaybackState(!!status);
        },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}
