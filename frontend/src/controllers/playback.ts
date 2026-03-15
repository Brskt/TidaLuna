import { sendIpc } from "../ipc";
import { setSelfLoad } from "../audio-proxy";
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

                // Fire MEDIA_PRODUCT_TRANSITION interceptors directly (not via store.dispatch
                // which triggers TIDAL's middleware and breaks the playback state machine).
                const productId = String(item.productId ?? item.id ?? "");
                if (productId && productId !== lastTransitionId) {
                    lastTransitionId = productId;
                    setTimeout(async () => {
                        try {
                            if (lastTransitionId !== productId) return;

                            const { interceptors } = require("../luna-core/exposeTidalInternals.patchAction");
                            const { store } = require("../luna-lib/redux/store");
                            const actionType = "playbackControls/MEDIA_PRODUCT_TRANSITION";

                            const controls = store.getState().playbackControls;
                            if (!controls) return;

                            // Get actual audio quality from playbackInfo API (same as TIDAL's SDK)
                            let actualQuality: string | undefined;
                            try {
                                const { getPlaybackInfo } = require("../luna-lib/helpers/getPlaybackInfo");
                                const state = store.getState();
                                const streamingQuality = state.settings?.quality?.streaming;

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

                                // Self-load non-FLAC BTS streams (TIDAL's boombox doesn't work in CEF)
                                console.log("[DBG:pbi] manifestMimeType=", pbi?.manifestMimeType, "manifest=", pbi?.manifest ? "present" : "null");
                                if (pbi?.manifestMimeType === "application/dash+xml" && pbi.manifest) {
                                    const manifest = pbi.manifest as any;
                                    if (manifest.initUrl && manifest.segmentUrls?.length > 0) {
                                        const codec = manifest.codec?.split(".")?.[0] ?? "aac";
                                        console.log("[DBG:pbi] DASH SELF-LOAD! codec=", codec, "segments=", manifest.segmentUrls.length);
                                        setSelfLoad(true);
                                        sendIpc("player.load_dash", manifest.initUrl, JSON.stringify(manifest.segmentUrls), codec);
                                    }
                                }
                                if (pbi?.manifestMimeType === "application/vnd.tidal.bts" && pbi.manifest) {
                                    const manifest = pbi.manifest;
                                    const streamUrl = manifest.urls?.[0];
                                    const mimeType: string = manifest.mimeType ?? "";
                                    console.log("[DBG:pbi] streamUrl=", streamUrl ? "present" : "null", "mimeType=", mimeType, "isFlac=", mimeType.includes("flac"));
                                    if (streamUrl && !mimeType.includes("flac")) {
                                        const codec = manifest.codecs?.split(".")?.[0] ?? "aac";
                                        const encKey = manifest.keyId ?? "";
                                        console.log("[DBG:pbi] SELF-LOAD! codec=", codec, "encKey=", encKey ? "present" : "empty");
                                        setSelfLoad(true);
                                        sendIpc("player.load", streamUrl, codec, encKey);
                                    }
                                }
                            } catch (e) { console.error("[luna:playback] playbackInfo/self-load error:", e); }

                            const oldCtx = controls.playbackContext ?? {};
                            const ctx = { ...oldCtx, actualProductId: productId, actualAudioQuality: actualQuality ?? oldCtx.actualAudioQuality, actualDuration: item.duration ?? oldCtx.actualDuration ?? 0, actualVideoQuality: null };
                            const oldMp = controls.mediaProduct ?? {};
                            const mp = { ...oldMp, productId, productType: item.type ?? "track" };

                            (window as any).__LUNAR_CURRENT_PRODUCT_ID__ = productId;

                            // Dispatch UPDATE_PLAYBACK_CONTEXT — Redux state is frozen, direct mutation fails
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
