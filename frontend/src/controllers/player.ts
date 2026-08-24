import { sendIpc, sendDbgIpc } from "../ipc";
import { setSelfLoad, isSelfLoad } from "../audio-proxy";
import type { AudioDevice } from "../types";

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
    let _lastVolume = -1;
    // Last mediaProduct.referenceId seen at load time. TIDAL mints a new referenceId
    // per play instance (spec: unique per MediaProduct instance); a change marks a
    // fresh play (restart at 0) vs a same-instance re-assert (keep position).
    let lastReferenceId: string | null = null;

    // A quality-swap re-load re-asserts the current product as a same-track
    // stop/load/play burst. Defer the stop a microtask; a same-(url,fmt)
    // load cancels it and skips the reload, leaving playback intact.
    let loadedUrl: string | null = null;
    let loadedFmt: string | null = null;
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
                setSelfLoad(false);
                seekTarget = null;
                // Fresh play instance? A changed mediaProduct.referenceId means the user
                // re-played this item; per the SDK load contract (native load = start at
                // 0) Rust must restart the committed track instead of resuming it. A same
                // referenceId (a quality-swap re-load) keeps its position.
                let restart = false;
                // The track this load is for. Read from the same mediaProduct object as
                // referenceId below, so it is exactly as current as the restart decision
                // this delegate already ships; Rust needs it to publish a measured length
                // under this track rather than whichever one announced itself first.
                let productId: string | null = null;
                try {
                    const { store } = require("../../plugins/lib/src/redux/store");
                    const mediaProduct = store?.getState?.()?.playbackControls?.mediaProduct;
                    productId = mediaProduct?.productId ?? null;
                    const refId = mediaProduct?.referenceId ?? null;
                    // Only a CHANGE from a known previous id is a fresh play; the first
                    // observation (lastReferenceId null) just records it, keeping the startup
                    // re-assert from being mistaken for a replay.
                    restart = lastReferenceId !== null && refId !== null && refId !== lastReferenceId;
                    if (refId !== null) lastReferenceId = refId;
                } catch (_) {}
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
                sendIpc("player.load", url, streamFormat, encryptionKey, restart, wantPlay, productId ?? "");
            },
            play: () => {
                sendDbgIpc("SDK->play");
                cancelDeferredStop();
                setDesired(true);
            },
            pause: () => {
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
            setVolume: (volume: number) => {
                // The SDK applies a perceptual curve before calling setVolume,
                // but we want the raw Redux value (0-100) for WASAPI session volume.
                try {
                    const { store } = require("../../plugins/lib/src/redux/store");
                    const raw = store?.getState()?.playbackControls?.volume;
                    if (raw !== undefined) volume = raw;
                } catch (_) {}
                if (volume === _lastVolume) return;
                _lastVolume = volume;
                sendIpc("player.volume", volume);
            },
            preload: (url: string, streamFormat: string, encryptionKey: string = "") => {
                // The delegate carries only the url triple, but the queue already names what
                // plays next. For SDK-native tracks the advance runs in Rust and nothing
                // re-tags them afterwards, which makes this the only chance to name one.
                // Self-load streams take the renderer path in index.ts and tag their own load.
                let nextId = "";
                try {
                    const { store } = require("../../plugins/lib/src/redux/store");
                    const { playQueue: q } = store.getState();
                    nextId = String(q?.elements?.[q.currentIndex + 1]?.mediaItemId ?? "");
                } catch (_) {}
                sendIpc("player.preload", url, streamFormat, encryptionKey, nextId);
            },
            cancelPreload: () => {
                sendIpc("player.preload.cancel");
            },
            recover: (...args: any[]) => {
                cancelDeferredStop();
                sendIpc("player.recover", ...args);
            },
            releaseDevice: () => {},
            selectDevice: (device: AudioDevice, mode: "shared" | "exclusive") => {
                // ASIO is sticky (it has no TIDAL device mode): a TIDAL re-assert with
                // "shared" (e.g. a track change) keeps ASIO running. But an explicit
                // "exclusive" selection is the user switching modes; it WINS over the
                // ASIO flag (clear it). Otherwise enabling exclusive while ASIO is on
                // gets re-forced back to ASIO and never switches.
                let effective: string;
                if (mode === "exclusive") {
                    if ((window as any).__TIDALUNAR_ASIO__ === true) {
                        (window as any).__TIDALUNAR_ASIO__ = false;
                        sendIpc("settings.asio", false);
                    }
                    effective = "exclusive";
                } else {
                    effective = (window as any).__TIDALUNAR_ASIO__ === true ? "asio" : "shared";
                }
                sendIpc("player.devices.set", device.id, effective);
            },
            selectSystemDevice: () => {
                if ((window as any).__TIDALUNAR_ASIO__ === true) {
                    sendIpc("player.devices.set", "auto", "asio");
                } else if ((window as any).__TIDALUNAR_EXCLUSIVE__ === true) {
                    sendIpc("player.devices.set", "auto", "exclusive");
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

    return {
        Player,
        activePlayer: playerHandle,
        syncBridgeVolume: (v: number) => { _lastVolume = v; },
        // Deduped play/pause intent for SDK tracks, driven by the Redux
        // playbackControls/PLAY|PAUSE interceptors (see index.ts).
        setDesiredPlayback: (playing: boolean) => setDesired(playing),
        // Set by a SELECT for the NEXT load to auto-play (see wantPlayOnLoad above);
        // clearPlayOnLoad drops it on a self-load path, which bypasses this delegate.
        requestPlayOnLoad: () => { wantPlayOnLoad = true; },
        clearPlayOnLoad: () => { wantPlayOnLoad = false; },
        trigger: (event: string, target: any, gen?: number) => {
            if (event !== "mediacurrenttime") {
                sendDbgIpc("trigger", event, target, "listeners=" + (activeEmitter?.listeners?.get(event)?.size ?? 0), "playerCalls=" + playerCallCount);
            }
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }

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
}
