import { sendIpc } from "../ipc";
import { setupActionHandlers, updateMetadata, updatePlaybackState } from "./mediasession";

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
            sendIpc("player.dbg", "registerDelegate", Object.keys(d || {}).join(","));
        },
        sendPlayerCommand: (cmd: any) => {
            sendIpc("player.dbg", "sendPlayerCommand", JSON.stringify(cmd));
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

                // Notify luna interceptors of the media transition.
                // We call interceptors directly instead of store.dispatch to avoid
                // TIDAL's internal middleware/reducers which break the playback state machine.
                // We also mutate playbackContext in the Redux state so that code reading
                // PlayState.playbackContext (e.g. updateFormat) sees the correct track ID.
                const productId = String(item.productId ?? item.id ?? "");
                if (productId && productId !== lastTransitionId) {
                    lastTransitionId = productId;
                    setTimeout(async () => {
                        try {
                            // Stale check: a newer transition supersedes this one
                            if (lastTransitionId !== productId) return;

                            const { interceptors } = require("../luna-core/exposeTidalInternals.patchAction");
                            const { store } = require("../luna-lib/redux/store");
                            const actionType = "playbackControls/MEDIA_PRODUCT_TRANSITION";

                            const controls = store.getState().playbackControls;
                            if (!controls) return;

                            // Query the playbackInfo API to get the actual audio quality
                            // the server delivers — mirrors TIDAL's native SDK which requests
                            // at the user's configured streaming quality, then reports back
                            // what was actually served in the response's audioQuality field.
                            let actualQuality: string | undefined;
                            try {
                                const { getPlaybackInfo } = require("../luna-lib/helpers/getPlaybackInfo");
                                const state = store.getState();
                                const streamingQuality = state.settings?.quality?.streaming;

                                // Try the user's streaming quality first. If the track doesn't
                                // support it (returns null), fall back to the track's own
                                // audioQuality from the content store.
                                console.log("[DBG:pbi] streamingQuality=", streamingQuality, "productId=", productId);
                                let pbi = await getPlaybackInfo(productId, streamingQuality);
                                console.log("[DBG:pbi] first call result=", pbi ? "OK" : "null", "quality=", pbi?.audioQuality);
                                if (!pbi && streamingQuality) {
                                    const trackItem = state.content?.mediaItems?.[productId]?.item ?? state.content?.tracks?.[productId];
                                    const fallbackQuality = trackItem?.audioQuality;
                                    console.log("[DBG:pbi] fallback trackItem=", trackItem ? "found" : "null", "fallbackQuality=", fallbackQuality);
                                    if (fallbackQuality && fallbackQuality !== streamingQuality) {
                                        pbi = await getPlaybackInfo(productId, fallbackQuality);
                                        console.log("[DBG:pbi] fallback result=", pbi ? "OK" : "null", "quality=", pbi?.audioQuality);
                                    }
                                }
                                if (lastTransitionId !== productId) return; // stale after async
                                actualQuality = pbi?.audioQuality;

                                // For non-FLAC formats (AAC/MP4), TIDAL's web SDK uses its own
                                // browser player (boombox) which doesn't work in CEF. We decode
                                // the BTS manifest to get the stream URL + encryption key, then
                                // load it ourselves — symphonia handles AAC/ISOMP4 natively.
                                console.log("[DBG:pbi] manifestMimeType=", pbi?.manifestMimeType, "manifest=", pbi?.manifest ? "present" : "null");
                                if (pbi?.manifestMimeType === "application/vnd.tidal.bts" && pbi.manifest) {
                                    const manifest = pbi.manifest;
                                    const streamUrl = manifest.urls?.[0];
                                    const mimeType: string = manifest.mimeType ?? "";
                                    console.log("[DBG:pbi] streamUrl=", streamUrl ? "present" : "null", "mimeType=", mimeType, "isFlac=", mimeType.includes("flac"));
                                    if (streamUrl && !mimeType.includes("flac")) {
                                        const codec = manifest.codecs?.split(".")?.[0] ?? "aac";
                                        const encKey = manifest.keyId ?? "";
                                        console.log("[DBG:pbi] SELF-LOAD! codec=", codec, "encKey=", encKey ? "present" : "empty");
                                        sendIpc("player.load", streamUrl, codec, encKey);
                                    }
                                }
                            } catch (e) { console.error("[luna:playback] playbackInfo/self-load error:", e); }

                            const oldCtx = controls.playbackContext ?? {};
                            const ctx = { ...oldCtx, actualProductId: productId, actualAudioQuality: actualQuality ?? oldCtx.actualAudioQuality, actualDuration: item.duration ?? oldCtx.actualDuration ?? 0, actualVideoQuality: null };
                            const oldMp = controls.mediaProduct ?? {};
                            const mp = { ...oldMp, productId, productType: item.type ?? "track" };

                            // Global override: updateFormat reads this instead of PlayState.playbackContext
                            (window as any).__LUNAR_CURRENT_PRODUCT_ID__ = productId;

                            // Update playbackContext in Redux so that PlayState.playbackContext
                            // returns the correct actualProductId. Redux Toolkit freezes state
                            // objects, so direct mutation fails — dispatch through TIDAL's own
                            // reducer which produces a new frozen state.
                            try {
                                const { buildActions } = require("../luna-core/exposeTidalInternals.patchAction");
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
        setCurrentTime: (time: any) => {
            sendIpc("player.dbg", "setCurrentTime", time);
            if (typeof time === 'number' && isFinite(time) && time >= 0) {
                (window.NativePlayerComponent as any)?._setTime?.(time);
            }
        },
        setPlayQueueState: (state: any) => {
            sendIpc("player.dbg", "setPlayQueueState", JSON.stringify(state));
        },
        setPlayingStatus: (status: any) => {
            sendIpc("player.dbg", "setPlayingStatus", JSON.stringify(status));
            (window as any).__TL_PLAYING__ = !!status;
            updatePlaybackState(!!status);
        },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}
