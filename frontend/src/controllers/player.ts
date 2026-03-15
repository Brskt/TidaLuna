import { sendIpc } from "../ipc";
import { setSelfLoad } from "../audio-proxy";
import type { AudioDevice } from "../types";

export const createNativePlayerComponent = () => {
    let activeEmitter: any = null;
    let activePlayer: any = null;
    let activeGen = 0;
    let playerCallCount = 0;
    // During a seek, holds the target position so that:
    // 1) the currentTime getter returns it immediately (for polling readers)
    // 2) stale bridge time events arriving before the backend catches up are
    //    blocked in processEvent() instead of reverting the seek bar.
    // Cleared once a bridge event close to the target arrives (±2s).
    let seekTarget: number | null = null;
    let _time = 0;

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
    // Returns false if the event was blocked (stale seek time).
    const processEvent = (event: string, target: any) => {
        if (event === "mediacurrenttime" && activePlayer) {
            if (seekTarget !== null) {
                if (Math.abs(target - seekTarget) > 2.0) return false;
                seekTarget = null;
            }
            activePlayer.currentTime = target;
        } else if (event === "mediaduration" && activePlayer) {
            activePlayer.duration = target;
        }
        activeEmitter?.emit?.(event, { target });
        return true;
    };

    const Player = () => {
        playerCallCount++;
        sendIpc("player.dbg", "Player() called", "count=" + playerCallCount);
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
            get currentTime() { return seekTarget ?? _time; },
            set currentTime(v: number) { _time = v; },
            duration: 0,
            addEventListener: (event: string, cb: any) => {
                sendIpc("player.dbg", "addEventListener", event);
                eventEmitter.addListener(event, cb);
            },
            removeEventListener: (event: string, cb: any) => eventEmitter.removeListener(event, cb),
            on: (event: string, cb: any) => {
                sendIpc("player.dbg", "on", event);
                eventEmitter.on(event, cb);
            },
            disableMQADecoder: () => {},
            enableMQADecoder: () => {},
            listDevices: () => {
                sendIpc("player.devices.get");
            },
            load: (url: string, streamFormat: string, encryptionKey: string = "") => {
                sendIpc("player.dbg", "SDK→load", streamFormat);
                setSelfLoad(false);
                seekTarget = null;
                // Soft-reset format data (don't drain resolvers — playback.ts already did)
                console.log("[DBG:player] load() soft-reset — __LUNAR_MEDIA_FORMAT__ = null (NO drain)");
                (window as any).__LUNAR_MEDIA_FORMAT__ = null;
                sendIpc("player.load", url, streamFormat, encryptionKey);
            },
            play: () => {
                sendIpc("player.dbg", "SDK→play");
                sendIpc("player.play");
            },
            pause: () => {
                sendIpc("player.pause");
            },
            stop: () => {
                sendIpc("player.stop");
            },
            seek: (time: number) => {
                sendIpc("player.dbg", "SDK→seek", time);
                seekTarget = time;
                _time = time;
                // Emit mediacurrenttime synchronously so the SDK layer (which
                // wraps this player) picks up the new position immediately.
                // This mirrors the official TIDAL SDK's nativePlayer.seek()
                // where this.currentTime = seconds is set before the actual seek.
                activeEmitter?.emit?.("mediacurrenttime", { target: time });
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
            recover: (...args: any[]) => {
                sendIpc("player.recover", ...args);
            },
            releaseDevice: () => {},
            selectDevice: (device: AudioDevice, mode: "shared" | "exclusive") => {
                sendIpc("player.devices.set", device.id, mode);
            },
            selectSystemDevice: () => {
                sendIpc("player.devices.set", 'auto');
            }
        };
        activePlayer = player;

        // Capture snapshot and clear — any new trigger() calls after this
        // point go through processEvent() directly via activeEmitter.
        const captured = snapshot;
        snapshot = null;

        // Replay snapshot events via chained setTimeout(0) so that each
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
                sendIpc("player.dbg", "snapshot replay", events.length + " events");
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
        // Internal setter for playback controller — updates _time without
        // emitting events or triggering backend seeks.  The currentTime getter
        // (seekTarget ?? _time) ensures the correct value is always returned.
        _setTime: (t: number) => { _time = t; },
        trigger: (event: string, target: any, gen?: number) => {
            if (event !== "mediacurrenttime") {
                sendIpc("player.dbg", "trigger", event, target, "listeners=" + (activeEmitter?.listeners?.[event]?.length ?? 0), "playerCalls=" + playerCallCount);
            }
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }

            // Before Player() — update snapshot with latest values.
            // Each event type overwrites the previous value, so only the
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
