import { sendIpc, sendDbgIpc } from "../ipc";
import { isSelfLoad, setSelfLoad, onHandoff, onProxyBind, onSdkLoad, onSdkReload, proxyFail, proxyLoading, proxyOwnerSrc } from "../audio-proxy";
import { forgetRefusal, refusePlayback } from "../refuse-playback";
import { onSdkStream, productOfStream } from "../sdk-playbackinfo";
import { setupActionHandlers, updateMetadata, updatePlaybackState } from "./mediasession";
import { setCurrentTrack } from "../video/cover-canvas";
import { storePublished } from "../store-published";
import type { ReceiverStream } from "../connect/types";

// `player.parse_dash` failure code for "no decoder here handles this codec", mirroring HTTP
// 415. Set in `src/ipc/plugin/mod.rs`; every other parse failure answers 400.
const UNDECODABLE_PROFILE = 415;

// The channel `getPlaybackInfo` rejects from when Rust cannot read a DASH manifest. The rejection
// carries it (`invokeIpcAs`), and only a DASH stream reaches that call.
const DASH_PARSE_CHANNEL = "player.parse_dash";

// `MediaItem.fromPlaybackContext` reads a null here as a track: a video needs a value, and nothing
// on this surface resolves video quality.
const VIDEO_QUALITY_UNRESOLVED = "UNKNOWN";

// How long the SDK's reload waits for the fresh credential `refreshCredential` asks for.
const CREDENTIAL_WAIT_MS = 3000;

// What `player.load` is handed for a BTS stream this page plays itself: the url, the codec, the key.
// `undefined` for a FLAC stream, which the SDK's native player loads.
const btsSelfLoad = (pbi: any) => {
    const manifest = pbi?.manifestMimeType === "application/vnd.tidal.bts" ? pbi.manifest : undefined;
    const streamUrl = manifest?.urls?.[0];
    if (!streamUrl || (manifest.mimeType ?? "").includes("flac")) return undefined;
    return { streamUrl, codec: manifest.codecs?.split(".")?.[0] ?? "aac", encKey: manifest.keyId ?? "" };
};

// Whose a failed load is, by the product the failure names. `current`: a track this page loaded
// itself and the listener still has. `superseded`: one it loaded itself and the listener has left.
// `foreign`: anything else, which the SDK loaded and TIDAL's native player reports.
export type LoadOwner = "current" | "superseded" | "foreign";

export const createPlaybackController = () => {
    let delegate: any = null;
    let lastTransitionId = "";
    // The last audio item announced: what a replay asks the load of again.
    let lastItem: any = null;
    // The product of the last self-load sent to Rust.
    let loadedId = "";
    // Whether that self-load is a BTS stream, which Rust keeps fetching with the url it was handed. A
    // DASH one is fetched whole before it plays and holds no url afterwards.
    let loadedProgressive = false;
    // TIDAL announces a track only when its current item changes; a track that starts again as
    // the very product Rust has loaded (ONE, ALL on a queue of one) is never announced, and nothing
    // would load it. Armed when the track ends or Shaka swaps to its preload, disarmed by the next
    // announcement: a transition to that product that comes first is such a replay. An announcement
    // already in flight arms nothing: it loads the track the transition is about.
    let disarm: () => void = () => {};
    const arm = () => {
        disarm();
        if (lastTransitionId !== loadedId) return;
        const { store } = require("../../plugins/lib/src/redux/store");
        const c0 = store.getState().playbackControls;
        const before = c0?.mediaProduct;
        const beforeCtx = c0?.playbackContext;
        const off = store.subscribe(() => {
            const controls = store.getState().playbackControls;
            if (controls?.mediaProduct === before && controls?.playbackContext === beforeCtx) return;
            disarm();
            if (String(controls?.mediaProduct?.productId ?? "") === loadedId) ctrl.replaySelfLoad();
        });
        disarm = () => {
            off();
            disarm = () => {};
        };
    };
    onHandoff(arm);
    // A refusal withholds the readiness of a stream the SDK still binds for the same announcement.
    // `refuse` sets it for every refusal; the next announcement clears it.
    let refused = false;
    // A refusal raises what its steps raised, after running them all. In the catch below that
    // would reach the catch of the whole notification and skip its context write: reported here.
    // It refuses the product last announced, which is the one every caller is loading.
    const refuse = (code: string, error: string, message: string) => {
        refused = true;
        try {
            refusePlayback(code, error, message, lastTransitionId);
        } catch (e) {
            console.error("[luna:playback] refusal failed:", e);
        }
    };
    // A track the SDK plays through Shaka sits on the element this page proxies, which only mirrors
    // Rust: when this page handed Rust nothing for it, the SDK's play resumes what Rust still holds
    // under this track's name. The element carries no product, but the SDK's own playbackinfo answer
    // does (`sdk-playbackinfo.ts`): the stream on the element names its track. `unloaded` is such a
    // track, settled once the element holds its stream or a newer announcement drops it. A FLAC
    // stream goes to the SDK's native player, never reaches the element and never settles.
    let unloaded: (() => boolean) | null = null;
    let announcements = 0;
    const settle = () => {
        if (unloaded?.()) unloaded = null;
    };
    onProxyBind((kind) => {
        console.log("[luna:playback] SDK bound", kind, "product", productOfStream(proxyOwnerSrc()) ?? "unknown", "announced", lastTransitionId);
        // A microtask: after the SDK's own call returns, and for a `src` before the readiness burst
        // queued behind it, which the refusal then withholds.
        if (unloaded) queueMicrotask(settle);
        else if (refused) queueMicrotask(proxyFail);
    });
    onSdkStream(settle);
    // The SDK loads the product of its own `mediaProduct`, which its prefill writes before any load. A
    // load the listener has since left (a pick, then another) binds its stream after the newer track
    // is loaded, with the store still on that newer one. The stream names its own product, and the SDK
    // records the length it is answered with for that product (the catalog's here; Rust may still be
    // loading the newer track, whose length is no answer to this one). A stream this page cannot
    // name (`productOfStream`) is taken for a reload of Rust's track.
    onSdkLoad((stream) => {
        const { store } = require("../../plugins/lib/src/redux/store");
        const state = store.getState();
        const current = String(state.playbackControls?.mediaProduct?.productId ?? "");
        const product = productOfStream(stream);
        if (product !== undefined && product !== current) {
            return { kind: "late", duration: Number(state.content?.mediaItems?.[product]?.item?.duration) || 0 };
        }
        return loadedId !== "" && current === loadedId ? { kind: "reload" } : { kind: "other" };
    });
    // The SDK reloads the track it plays when its own stream expired (a Play long after the pause) or was
    // loaded paused, and answers itself from Rust's track; Rust is sent nothing. Rust keeps streaming
    // a BTS track with the signed url it was handed, and takes a new one only from a `player.load` of the
    // same track, which the SDK never sends for a track it did not load: its reconnect would meet a 403.
    // A fresh answer is asked for and handed over, keeping the position (no restart, no play intent).
    const askForCredential = async (productId: string) => {
        try {
            const { getPlaybackInfo } = require("../../plugins/lib/src/helpers/getPlaybackInfo");
            const { store } = require("../../plugins/lib/src/redux/store");
            const fresh = btsSelfLoad(await getPlaybackInfo(productId, store.getState().settings?.quality?.streaming));
            // Left behind during the request: the listener moved on, or the SDK loaded something itself.
            if (!fresh || loadedId !== productId || lastTransitionId !== productId || !isSelfLoad()) return;
            sendIpc("player.load", fresh.streamUrl, fresh.codec, fresh.encKey, false, false, productId);
        } catch (e) {
            console.error("[luna:playback] credential refresh failed:", e);
        }
    };
    // Waited for by the answer to the SDK, whose play follows it: a re-arm of Rust's track at that play
    // would otherwise use the old url. Bounded: a request stuck behind the rate gate cannot hold the
    // play. Past it the SDK is answered, and the credential still goes out when it arrives.
    const refreshCredential = async () => {
        const productId = loadedId;
        if (!loadedProgressive || productId === "") return;
        let timer: ReturnType<typeof setTimeout> | undefined;
        const stuck = new Promise<void>((resolve) => (timer = setTimeout(resolve, CREDENTIAL_WAIT_MS)));
        try {
            await Promise.race([askForCredential(productId), stuck]);
        } finally {
            clearTimeout(timer);
        }
    };
    onSdkReload(refreshCredential);
    const ctrl = {
        // Rust names the product of a load that failed before its track played. The SDK knows
        // nothing of a track this page loaded itself; the failure is this page's to answer,
        // and only while the listener still has that track. The self-load flag says the last load
        // was this page's: an SDK load after it is no self-load, whatever id it carries.
        loadOwner(productId: unknown): LoadOwner {
            if (!isSelfLoad() || loadedId === "" || String(productId ?? "") !== loadedId) return "foreign";
            return lastTransitionId === loadedId ? "current" : "superseded";
        },
        // The same track again, with nothing to announce it: TIDAL announces only when the current
        // item changes, and the guard below drops a repeated id.
        replaySelfLoad() {
            if (!lastItem) return;
            lastTransitionId = "";
            this.setCurrentMediaItem(lastItem);
        },
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
        // TIDAL calls this with the item alone. `alreadyPlaying` is for a track something else
        // has loaded: the TIDAL Connect receiver announces the one it plays through here, and Rust
        // refuses a self-load of it during the cast, stopping this page's transport under a banner.
        // `stream` is what that loader resolved, the only source of the format the track plays at:
        // this page's own quality setting says nothing about a stream it did not request.
        setCurrentMediaItem: (
            item: any,
            { alreadyPlaying = false, stream = null }: { alreadyPlaying?: boolean; stream?: ReceiverStream | null } = {},
        ) => {
            if (item && typeof item === "object") {
                if (item.type !== "video") lastItem = item;
                // The one signal that fires for EVERY track, animated cover or not.
                // `setCurrentTrack` takes the still artwork from it (see its doc).
                setCurrentTrack(item);

                // Send raw media item; metadata parsing/fallbacks are centralized in Rust.
                sendIpc("player.metadata", item);
                updateMetadata(item);

                // Fire MEDIA_PRODUCT_TRANSITION interceptors directly (not via store.dispatch
                // which triggers TIDAL's middleware and breaks the playback state machine).
                const productId = String(item.productId ?? item.id ?? "");
                if (productId && productId !== lastTransitionId) {
                    lastTransitionId = productId;
                    const announcement = ++announcements;
                    unloaded = null;
                    refused = false;
                    forgetRefusal(productId);
                    disarm();
                    proxyLoading();
                    setTimeout(async () => {
                        try {
                            if (lastTransitionId !== productId) return;

                            await storePublished();
                            if (lastTransitionId !== productId) return;

                            const { interceptors } = require("../../render/src/exposeTidalInternals.patchAction");
                            const { store } = require("../../plugins/lib/src/redux/store");
                            const actionType = "playbackControls/MEDIA_PRODUCT_TRANSITION";

                            const controls = store.getState().playbackControls;
                            if (!controls) return;

                            // Videos come through this delegate too, and `desktop.tidal.com` has
                            // no video playback info (`/v1/tracks/<videoId>/playbackinfo` answers
                            // 500). Leaving `pbi` undefined also skips the self-load blocks below.
                            const isVideo = item.type === "video";

                            // Whether this page handed Rust the track, or refused it itself.
                            let handed = false;
                            let refusedHere = false;

                            // Get actual audio quality from playbackInfo API (same as TIDAL's SDK)
                            let actualQuality: string | undefined;
                            let pbi: any;
                            try {
                                const { getPlaybackInfo } = require("../../plugins/lib/src/helpers/getPlaybackInfo");
                                const state = store.getState();
                                const streamingQuality = state.settings?.quality?.streaming;

                                if (!isVideo && !alreadyPlaying) {
                                    pbi = await getPlaybackInfo(productId, streamingQuality);
                                    if (!pbi && streamingQuality) {
                                        const trackItem = state.content?.mediaItems?.[productId]?.item ?? state.content?.tracks?.[productId];
                                        const fallbackQuality = trackItem?.audioQuality;
                                        if (fallbackQuality && fallbackQuality !== streamingQuality) {
                                            pbi = await getPlaybackInfo(productId, fallbackQuality);
                                        }
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
                                        loadedId = productId;
                                        loadedProgressive = false;
                                        // Self-load bypasses the SDK load delegate; drop any pending
                                        // select-play intent, keeping it out of a later FLAC load.
                                        (window.NativePlayerComponent as any)?.clearPlayOnLoad?.();
                                        sendIpc("player.load_dash", manifest.initUrl, JSON.stringify(manifest.segmentUrls), codec, productId);
                                        handed = true;
                                    }
                                }
                                if (pbi?.manifestMimeType === "application/vnd.tidal.bts" && pbi.manifest) {
                                    const self = btsSelfLoad(pbi);
                                    if (self) {
                                        const { streamUrl, codec, encKey } = self;
                                        setSelfLoad(true);
                                        loadedId = productId;
                                        loadedProgressive = true;
                                        // Self-load bypasses the SDK load delegate; drop any pending
                                        // select-play intent, keeping it out of a later FLAC load.
                                        (window.NativePlayerComponent as any)?.clearPlayOnLoad?.();
                                        // restart/wantPlay repeat the defaults this call has
                                        // always been parsed with; they are spelled out only
                                        // so productId lands on the index player.load reads.
                                        sendIpc("player.load", streamUrl, codec, encKey, false, false, productId);
                                        handed = true;
                                    }
                                }
                            } catch (e) {
                                console.error("[luna:playback] playbackInfo/self-load error:", e);
                                // A rejection is a resumption too, and it reached here by
                                // skipping the check above. Returning covers the tail as well,
                                // which would otherwise write this track's identity over the
                                // current one. The log stays: it names a real failure either way.
                                if (lastTransitionId !== productId) return;
                                // A manifest Rust cannot read is DASH, which the SDK plays through
                                // the element this page proxies. Only this page could have
                                // loaded the track, and the SDK's play would resume what Rust
                                // still holds under this track's name. Every other failure stays
                                // silent: it does not say what the SDK loads (a FLAC stream goes
                                // to the native player, which a refusal would stop) nor whether
                                // the SDK's own request failed too, and a network hiccup is not
                                // a quality problem.
                                const failure = e as { code?: number; channel?: string };
                                if (failure?.code === UNDECODABLE_PROFILE) {
                                    // 415 is the failure a listener can act on: no decoder for
                                    // this codec. Rust names it for the console, the banner
                                    // below carries the human sentence.
                                    // Deliberately no `player.pause`. Answering "paused" to a
                                    // TIDAL that just asked to play reads as a failed start,
                                    // and it advances the queue; measured on two tracks.
                                    refusedHere = true;
                                    refuse(
                                        "tidalunar_undecodable_profile",
                                        "stream codec not supported by this build",
                                        "TidaLunar cannot play music below 320 kbps for the moment. Please change the quality in TIDAL's settings.",
                                    );
                                } else if (failure?.channel === DASH_PARSE_CHANNEL) {
                                    refusedHere = true;
                                    refuse(
                                        "tidalunar_manifest_unreadable",
                                        "stream manifest rejected by this build",
                                        "TidaLunar could not load this track. Please try again or pick another one.",
                                    );
                                }
                            }

                            // Every way this page can come out without having loaded the track lands
                            // here: a failed request, no answer, an unreadable one, an incomplete one.
                            // Whether it is a failure is the element's to say, not this page's.
                            if (!handed && !refusedHere && !isVideo && !alreadyPlaying) {
                                unloaded = () => {
                                    if (announcement !== announcements) return true;
                                    if (productOfStream(proxyOwnerSrc()) !== productId) return false;
                                    refuse(
                                        "tidalunar_manifest_unreadable",
                                        "stream could not be loaded by this page",
                                        "TidaLunar could not load this track. Please try again or pick another one.",
                                    );
                                    return true;
                                };
                                settle();
                            }

                            const oldCtx = controls.playbackContext ?? {};
                            // A track something else plays is described by that loader's answer alone.
                            // Without one the format is unknown, which is not the previous track's.
                            const format = alreadyPlaying ? (stream ?? {}) : pbi;
                            const carried = <T>(previous: T) => (alreadyPlaying ? null : previous);
                            // Fields fall back to `oldCtx` until resolved, yet TIDAL's quality
                            // menu reads them whatever the kind: a video inheriting them would
                            // show the previous track's. Each kind sets every field.
                            const ctx = {
                                ...oldCtx,
                                actualProductId: productId,
                                actualDuration: item.duration ?? oldCtx.actualDuration ?? 0,
                                actualVideoQuality: isVideo ? VIDEO_QUALITY_UNRESOLVED : null,
                                actualAudioQuality: isVideo ? null : (actualQuality ?? format?.audioQuality ?? carried(oldCtx.actualAudioQuality)),
                                bitDepth: isVideo ? null : (format?.bitDepth ?? carried(oldCtx.bitDepth ?? null)),
                                sampleRate: isVideo ? null : (format?.sampleRate ?? carried(oldCtx.sampleRate ?? null)),
                            };
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
    };
    return ctrl;
}
