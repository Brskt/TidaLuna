import { sendIpc, sendDbgIpc } from "../ipc";
import { setSelfLoad, isSelfLoad } from "../audio-proxy";
import { applyVolume } from "../volume";
import { deviceReleasedToShaka, selectDevice } from "../output-device";
import type { AudioDevice } from "../types";
import { productOfStream } from "../sdk-playbackinfo";

export type TrackListPayload = { position?: number | string; context?: { id?: unknown; type?: string; isAutoPlay?: boolean } } | undefined;

/**
 * What a `playQueue/ADD_TRACK_LIST_TO_PLAY_QUEUE` asks of the load that follows it, `null` for
 * nothing. Only "now" (or an unknown shape) carries play intent; "next"/"last"/index are queue
 * edits with no load, and arming on them would force-resume a paused track later. A "now" is a
 * listener's pick, except the mix TIDAL starts itself when a queue runs out (`context.isAutoPlay`).
 */
export const intentOfTrackList = (p: TrackListPayload): "play" | "pick" | null => {
    const position = p?.position;
    if (position !== undefined && position !== "now") return null;
    return p?.context?.isAutoPlay ? "play" : "pick";
};

/**
 * A listener's act on this page's play queue that TIDAL answers with a load of the entry it lands
 * on. During a TIDAL Connect cast, Rust plays it on the cast's queue or refuses it (`CastRoute`).
 */
export type ListenerAct = "pick" | "next" | "previous";

/**
 * A listener's act and the play queue entries it acted on, named by their `context` object:
 * every entry TIDAL makes for one play-now carries that action's own `context`, and every copy
 * TIDAL makes of an entry keeps it. An entry made by anything else carries another one.
 */
export type QueueAct = { act: ListenerAct; context: object };

type PlayQueueState = { currentIndex?: number; elements?: { uid?: unknown; context?: unknown }[] } | undefined;

const isObject = (v: unknown): v is object => typeof v === "object" && v !== null;

/** The pick a play-now is, bound to the entries TIDAL makes for it; `null` for any other. */
export const pickOf = (p: TrackListPayload): QueueAct | null =>
    intentOfTrackList(p) === "pick" && isObject(p?.context) ? { act: "pick", context: p.context } : null;

/**
 * A Next or Previous, bound to the entries of the queue's current one, read before TIDAL moves
 * the queue. During a cast that queue is the cast's track alone, so the move reloads that entry.
 */
export const moveOf = (act: "next" | "previous", queue: PlayQueueState): QueueAct | null => {
    const context = queue?.elements?.[queue?.currentIndex ?? -1]?.context;
    return isObject(context) ? { act, context } : null;
};

/**
 * The act a load answers: `pending`'s, when the entry loaded (the one whose uid is the load's
 * `referenceId`) is one the act was on, and none otherwise. Only a load of one of its own
 * entries counts for an act, and no other load may spend it: a pick whose load never came would
 * otherwise go to TIDAL's next automatic load and restart the cast's track.
 */
export const actOfLoad = (pending: QueueAct | null, queue: PlayQueueState, referenceId: unknown): ListenerAct | null => {
    if (pending === null || referenceId === null || referenceId === undefined) return null;
    const entry = queue?.elements?.find((e) => e?.uid === referenceId);
    return entry !== undefined && entry.context === pending.context ? pending.act : null;
};

/**
 * What Rust's refusal of a load it did not run makes of the SDK's latest load (see `refuseLoad`).
 * Only a pick is told why nothing plays: TIDAL loads on its own during a cast too (the cast's
 * track after each change of it, a restore), and a banner then answers a question no one asked.
 */
export type LoadRefusal =
    // Answers an earlier load (see loadId).
    | { kind: "stale" }
    | { kind: "pick" }
    // The reload of a listener's Next or Previous: Rust steps the cast as the controller's own
    // step would (at once, or not at all with no queue loaded or at its end). Settled with the
    // catalog's `duration`: at 0, TIDAL's play step reads the time as past the length and
    // reloads again, which at the queue's end is the cast's own track, and its play would resume
    // a paused cast.
    | { kind: "moved"; duration: number }
    // A load TIDAL made on its own, for `productId`.
    | { kind: "automatic"; productId: string | null };

export const createNativePlayerComponent = () => {
    let activeEmitter: any = null;
    let activePlayer: any = null;
    let activeGen = 0;
    let playerCallCount = 0;
    // During a seek, holds the target: the currentTime getter returns it immediately
    // (for polling readers) before the first bridge event lands. Suppressing pre-seek
    // positions is Rust's job: every backend reports the seek target until its decoder
    // converges. The first event to arrive here is already the truth.
    let seekTarget: number | null = null;
    let _time = 0;
    // Last mediaProduct.referenceId seen at load time. TIDAL mints a new referenceId
    // per play instance (spec: unique per MediaProduct instance); a change marks a
    // fresh play (restart at 0) vs a same-instance re-assert (keep position).
    let lastReferenceId: string | null = null;

    // A quality-swap re-load re-asserts the current product as a same-track
    // stop/load/play burst. Defer the stop a microtask; a same-(url,fmt)
    // load cancels it and skips the reload, leaving playback intact.
    let loadedUrl: string | null = null;
    let loadedFmt: string | null = null;
    // The stream Rust has staged as next. The SDK loads it again to switch to it, and that load is no
    // fresh play of it.
    let preloaded: { url: string } | null = null;
    // The SDK's native player parks itself while another player is the active one: its `next()` calls
    // `preload` and then `pause` in the same synchronous run. That pause is the only one that is not the
    // listener's, and it is told apart by what comes right before it, not by who plays now.
    let parkingNative = false;
    let pendingStop = false;
    // A real command (play/pause/seek/recover) supersedes a deferred stop.
    const cancelDeferredStop = () => { pendingStop = false; };

    // Desired playback intent, deduped, routed here from both the SDK delegate and the
    // Redux PLAY/PAUSE actions: TIDAL sometimes resumes a paused track via stop()+load(same)
    // WITHOUT calling play(). Relying on the SDK delegate alone misses it. null = unknown
    // (forces the next emit); a genuine load() resets it for a queue advance to re-emit.
    let desiredPlaying: boolean | null = null;
    const setDesired = (playing: boolean) => {
        if (desiredPlaying === playing) return;
        desiredPlaying = playing;
        sendIpc(playing ? "player.play" : "player.pause");
    };
    // A track/list SELECT (ADD_TRACK_LIST) sets this; the load delegate folds it into
    // player.load as the auto_play flag: the SELECTED track plays atomically with its
    // load, never a separate player.play that would resume the still-committed OLD paused
    // track (the "bleed"). Set on select, consumed/cleared per load.
    let wantPlayOnLoad = false;
    // The listener's latest act on the play queue, until a load of its own entries answers it
    // (see actOfLoad). Apart from wantPlayOnLoad because the two age differently: the play
    // intent is spent by the next load, whichever it is.
    let pendingAct: QueueAct | null = null;
    // The number of the SDK's latest load, sent with it. Rust's answer to a load it did not run
    // names the load it answers, and one naming an earlier load is stale: acting on it would
    // settle or refuse the newer load instead.
    let loadId = 0;
    // The track the SDK's latest load is for, which a settled answer takes its length from.
    let loadProductId: string | null = null;
    // The listener's act the SDK's latest load answers, which a refusal of it goes by.
    let loadAct: ListenerAct | null = null;
    // The player state Rust last reported. A load Rust did not run leaves the player in it, while
    // the SDK's load set its own to NOT_PLAYING as it began, and told TIDAL's store and the OS.
    let reportedState: unknown = null;
    // Set when Rust refused the SDK's last load, cleared by its next. The SDK's autoplay still
    // `play()`s the refused track, which Rust would take as resuming the cast's. A listener's own
    // Play comes through TIDAL's PLAY action and `setDesiredPlayback`, not here.
    let loadRefused = false;
    // The length TIDAL's own pick saga announces the SDK's latest load's track with: the catalog's.
    const catalogLength = (): number => {
        try {
            const { store } = require("../../plugins/lib/src/redux/store");
            const item = store?.getState?.()?.content?.mediaItems?.[String(loadProductId)]?.item;
            return Number(item?.duration) || 0;
        } catch (_) {
            return 0;
        }
    };

    // Stable handle published on tidalModules for @luna/lib PlayState.currentTime to resolve.
    const playerHandle = {
        get currentTime(): number { return seekTarget ?? _time; },
    };

    // Before Player() is called, events are captured as a snapshot of the
    // latest values rather than queued individually.  This avoids unbounded
    // growth and makes replay order deterministic.
    let snapshot: {
        state: string | null;
        duration: number | null;
        time: number | null;
        format: any | null;
        passthrough: { event: string; target: any }[];
    } | null = {
        state: null,
        duration: null,
        time: null,
        format: null,
        passthrough: [],
    };

    // Shared event processing: updates player state and emits to listeners.
    const processEvent = (event: string, target: any) => {
        if (event === "mediacurrenttime" && activePlayer) {
            // Take every position at face value. Filtering on distance from the pin made a
            // REFUSED seek indistinguishable from a stale one. Only a close-enough value
            // cleared the pin, leaving the refusal's true position dropped and the pin held
            // forever, freezing the SDK's position of record.
            seekTarget = null;
            activePlayer.currentTime = target;
        } else if (event === "mediaduration" && activePlayer) {
            activePlayer.duration = target;
        }
        activeEmitter?.emit?.(event, { target });
        return true;
    };

    const Player = () => {
        playerCallCount++;
        sendDbgIpc("Player() called", "count=" + playerCallCount);

        // Release the previous emitter's listeners before it is orphaned.  The
        // old player object returned to the SDK still closes over its emitter;
        // without this the SDK's accumulated callbacks (and the React state
        // they capture) stay reachable for the whole session.
        activeEmitter?.listeners.clear();

        const eventEmitter = {
            // One Set per event type: Set.add deduplicates by callback
            // reference, matching the WHATWG addEventListener contract. The
            // SDK re-subscribing on every render no longer stacks listeners.
            listeners: new Map<string, Set<Function>>(),
            addListener(event: string, cb: any) {
                let set = this.listeners.get(event);
                if (!set) this.listeners.set(event, (set = new Set()));
                set.add(cb);
            },
            removeListener(event: string, cb: any) {
                this.listeners.get(event)?.delete(cb);
            },
            on(event: string, cb: any) {
                this.addListener(event, cb);
            },
            emit(event: string, arg: any) {
                const set = this.listeners.get(event);
                if (!set || set.size === 0) return;
                if (set.size === 1) {
                    // Hot path (e.g. mediacurrenttime usually has one listener):
                    // read the single callback up front, blocking re-entry
                    // mid-dispatch, and skip the snapshot allocation.
                    const only = set.values().next().value;
                    if (only) only(arg);
                    return;
                }
                // Snapshot: a listener added or removed during dispatch
                // doesn't affect the current pass.
                for (const cb of [...set]) cb(arg);
            }
        };

        activeEmitter = eventEmitter;

        const player = {
            get currentTime() { return seekTarget ?? _time; },
            set currentTime(v: number) { _time = v; },
            duration: 0,
            addEventListener: (event: string, cb: any) => {
                sendDbgIpc("addEventListener", event);
                eventEmitter.addListener(event, cb);
            },
            removeEventListener: (event: string, cb: any) => eventEmitter.removeListener(event, cb),
            on: (event: string, cb: any) => {
                sendDbgIpc("on", event);
                eventEmitter.on(event, cb);
            },
            disableMQADecoder: () => {},
            enableMQADecoder: () => {},
            listDevices: () => {
                sendIpc("player.devices.get");
            },
            load: (url: string, streamFormat: string, encryptionKey: string = "") => {
                sendDbgIpc("SDK->load", streamFormat);
                // Echo of the current track: skip the reload. !isSelfLoad() because
                // self-loads bypass this delegate, leaving loadedUrl/loadedFmt stale.
                if (pendingStop && !isSelfLoad() && url === loadedUrl && streamFormat === loadedFmt) {
                    pendingStop = false;
                    wantPlayOnLoad = false; // don't let a stray select-play intent survive to a later load
                    return;
                }
                // Genuine load (different track/quality): flush the deferred stop first.
                if (pendingStop) {
                    pendingStop = false;
                    sendIpc("player.stop");
                }
                loadedUrl = url;
                loadedFmt = streamFormat;
                loadId++;
                loadRefused = false;
                setSelfLoad(false);
                seekTarget = null;
                // Fresh play instance? A changed mediaProduct.referenceId means the user
                // re-played this item; per the SDK load contract (native load = start at
                // 0) Rust must restart the committed track instead of resuming it. A same
                // referenceId (a quality-swap re-load) keeps its position.
                let restart = false;
                // The track this load is for; Rust needs it to publish a measured length under this
                // track rather than whichever one announced itself first. Named by the stream below,
                // and the store's product only stands in for a stream this page saw no answer for.
                let productId: string | null = null;
                let act: ListenerAct | null = null;
                try {
                    const { store } = require("../../plugins/lib/src/redux/store");
                    const state = store?.getState?.();
                    const mediaProduct = state?.playbackControls?.mediaProduct;
                    productId = mediaProduct?.productId ?? null;
                    const refId = mediaProduct?.referenceId ?? null;
                    // Only a CHANGE from a known previous id is a fresh play; the first
                    // observation (lastReferenceId null) just records it, keeping the startup
                    // re-assert from being mistaken for a replay.
                    restart = lastReferenceId !== null && refId !== null && refId !== lastReferenceId;
                    if (refId !== null) lastReferenceId = refId;
                    act = actOfLoad(pendingAct, state?.playQueue, refId);
                } catch (_) {}
                // The load that switches to the staged track is no fresh play of it: Rust advanced to
                // it by itself, and a restart would rewind it.
                if (preloaded?.url === url) {
                    restart = false;
                }
                preloaded = null;
                // The SDK's playbackinfo answer the stream was parsed from names its track. The store
                // cannot: it holds the track that just ended when the SDK switches to a preloaded one.
                productId = productOfStream(url) ?? productId;
                loadProductId = productId;
                if (act !== null) pendingAct = null;
                loadAct = act;
                // On a restart, pin our reported head to 0 to match the position the
                // SDK expects after a load, closing the mismatch that drives its seek loop.
                if (restart) _time = 0;
                // Fold the SELECT's play-intent into this load: the newly-selected track
                // auto-plays atomically with its load (no separate player.play that would
                // resume the still-committed OLD paused track). Consume-and-clear.
                const wantPlay = wantPlayOnLoad;
                wantPlayOnLoad = false;
                // desiredPlaying reflects reality: a wantPlay load ends PLAYING (letting a
                // later redundant SDK play() dedup); a plain load stays null for a real later
                // play to still re-emit (queue advance).
                desiredPlaying = wantPlay ? true : null;
                // Soft-reset format data (don't drain resolvers - playback.ts already did)
                (window as any).__LUNAR_MEDIA_FORMAT__ = null;
                // First fresh time report must not be held behind the 250ms throttle
                // (armed against the OLD track's last dispatch); mirrors SEEK.
                (window as any).__LUNAR_FORCE_TIME_DISPATCH__?.();
                sendIpc("player.load", url, streamFormat, encryptionKey, restart, wantPlay, productId ?? "", loadId, act ?? "");
            },
            play: () => {
                sendDbgIpc("SDK->play");
                // The play of a track Rust never loaded: it can only resume the cast's.
                if (loadRefused) return;
                cancelDeferredStop();
                setDesired(true);
            },
            pause: () => {
                // The SDK parking the native player as it preloads a track into it, while another
                // player is the active one: no pause of Rust's track.
                if (parkingNative) return;
                cancelDeferredStop();
                setDesired(false);
            },
            stop: () => {
                // Deferred for the echo's synchronous load() to cancel it; a genuine stop
                // flushes here and forgets the track, leaving a later re-select to reload.
                pendingStop = true;
                queueMicrotask(() => {
                    if (!pendingStop) return;
                    pendingStop = false;
                    loadedUrl = null;
                    loadedFmt = null;
                    sendIpc("player.stop");
                });
            },
            seek: (time: number) => {
                sendDbgIpc("SDK->seek", time);
                cancelDeferredStop();
                seekTarget = time;
                _time = time;
                // Emit mediacurrenttime synchronously: the SDK layer (which
                // wraps this player) picks up the new position immediately.
                // This mirrors the official TIDAL SDK's nativePlayer.seek()
                // where this.currentTime = seconds is set before the actual seek.
                activeEmitter?.emit?.("mediacurrenttime", { target: time });
                sendIpc("player.seek", time);
            },
            setVolume: (volume: number) => applyVolume(volume),
            preload: (url: string, streamFormat: string, encryptionKey: string = "") => {
                // The delegate carries only the url triple, but the SDK's playbackinfo answer the
                // stream was parsed from names its track. For SDK-native tracks the advance runs in
                // Rust and nothing re-tags them afterwards, which makes this the only chance to
                // name one. Self-load streams take the renderer path in index.ts and tag their own
                // load. A stream that answer never named goes untagged: no tag publishes no length.
                const nextId = productOfStream(url) ?? "";
                if (nextId === "") console.warn("[luna:player] preload of a stream no playbackinfo answer named");
                preloaded = { url };
                parkingNative = true;
                queueMicrotask(() => { parkingNative = false; });
                sendIpc("player.preload", url, streamFormat, encryptionKey, nextId);
            },
            cancelPreload: () => {
                // Not gated on isSelfLoad() like stop: the staged next is this player's alone (`preload`
                // above is its only producer), and it is dropped whoever plays now. Held back during a
                // self-load, a stale staged next would be promoted when that track ends.
                preloaded = null;
                sendIpc("player.preload.cancel");
            },
            recover: (...args: any[]) => {
                cancelDeferredStop();
                sendIpc("player.recover", ...args);
            },
            releaseDevice: () => {},
            selectDevice: (device: AudioDevice, mode: "shared" | "exclusive") => selectDevice(device.id, mode),
            selectSystemDevice: () => {
                // Rust plays shaka's tracks too: releasing the lock keeps the device, shared.
                // The system device itself takes no mode: TIDAL's native player asks for it with
                // none, and its dialog offers exclusive only on a chosen device.
                const released = deviceReleasedToShaka();
                if ((window as any).__TIDALUNAR_ASIO__ === true) {
                    sendIpc("player.devices.set", "auto", "asio");
                } else if (released !== null) {
                    selectDevice(released, "shared");
                } else {
                    sendIpc("player.devices.set", "auto");
                }
            }
        };
        activePlayer = player;

        // Capture snapshot and clear - any new trigger() calls after this
        // point go through processEvent() directly via activeEmitter.
        const captured = snapshot;
        snapshot = null;

        // Replay snapshot events via chained setTimeout(0): each
        // event fires in its own macrotask.  This gives the SDK's async
        // handlers (which use `await nativeEvent('mediaduration')` then
        // `await mediaStateChange('active')`) a chance to resolve their
        // Promises and register next listeners between events.
        if (captured) {
            const events: [string, any][] = [];
            for (const e of captured.passthrough) events.push([e.event, e.target]);
            if (captured.format !== null) events.push(["mediaformat", captured.format]);
            if (captured.duration !== null) events.push(["mediaduration", captured.duration]);
            if (captured.state !== null) events.push(["mediastate", captured.state]);
            if (captured.time !== null) events.push(["mediacurrenttime", captured.time]);

            if (events.length > 0) {
                sendDbgIpc("snapshot replay", events.length + " events");
                let idx = 0;
                const step = () => {
                    if (idx < events.length) {
                        processEvent(events[idx][0], events[idx][1]);
                        idx++;
                        setTimeout(step, 0);
                    }
                };
                setTimeout(step, 0);
            }
        }

        return player;
    }

    const component = {
        Player,
        activePlayer: playerHandle,
        // Deduped play/pause intent for SDK tracks, driven by the Redux
        // playbackControls/PLAY|PAUSE interceptors (see index.ts).
        setDesiredPlayback: (playing: boolean) => setDesired(playing),
        // Set by a SELECT for the NEXT load to auto-play (see wantPlayOnLoad above);
        // clearPlayOnLoad drops it on a self-load path, which bypasses this delegate.
        requestPlayOnLoad: () => { wantPlayOnLoad = true; },
        clearPlayOnLoad: () => { wantPlayOnLoad = false; },
        // A listener's act on the play queue, for the load of its own entries (see pendingAct).
        actOnQueue: (act: QueueAct) => { pendingAct = act; },
        // Rust's answers to a load it did not run during a cast (see loadId).
        // Refused (`mediacastowned`): see loadRefused and LoadRefusal.
        refuseLoad: (id: unknown): LoadRefusal => {
            if (id !== loadId) return { kind: "stale" };
            loadRefused = true;
            switch (loadAct) {
                case "pick":
                    return { kind: "pick" };
                case "next":
                case "previous":
                    return { kind: "moved", duration: catalogLength() };
                case null:
                    return { kind: "automatic", productId: loadProductId };
            }
        },
        // Played by the controller's queue (`mediacastrouted`): the load stands as one that ran,
        // with the catalog's length. Null for a stale answer.
        routeLoad: (id: unknown): number | null => (id === loadId ? catalogLength() : null),
        // Ends the SDK's wait on a load Rust did not run and gives its player back the state Rust
        // last reported (see reportedState). A refused pick is not settled here: its refusal
        // stops the transport on purpose.
        settleLoad: (duration: number) => {
            component.trigger("mediaduration", duration);
            if (reportedState !== null) component.trigger("mediastate", reportedState);
        },
        trigger: (event: string, target: any, gen?: number) => {
            if (event !== "mediacurrenttime") {
                sendDbgIpc("trigger", event, target, "listeners=" + (activeEmitter?.listeners?.get(event)?.size ?? 0), "playerCalls=" + playerCallCount);
            }
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }
            if (event === "mediastate") reportedState = target;

            // Before Player(): update snapshot with latest values.
            // Each event type overwrites the previous value; only the
            // most recent state is kept (no unbounded growth).
            if (snapshot) {
                if (event === "mediacurrenttime") {
                    snapshot.time = target;
                } else if (event === "mediaduration") {
                    snapshot.duration = target;
                } else if (event === "mediastate") {
                    snapshot.state = target;
                } else if (event === "mediaformat") {
                    snapshot.format = target;
                } else {
                    // Passthrough events: keep only latest per event type.
                    const idx = snapshot.passthrough.findIndex(e => e.event === event);
                    if (idx >= 0) snapshot.passthrough[idx].target = target;
                    else snapshot.passthrough.push({ event, target });
                }
                return;
            }

            processEvent(event, target);
        }
    };
    return component;
}
