import { sendIpc } from "../ipc";
import type { AudioDevice } from "../types";

export const createNativePlayerComponent = () => {
    let activeEmitter: any = null;
    let activePlayer: any = null;
    let activeGen = 0;
    // During a seek, holds the target position so that:
    // 1) the currentTime getter returns it immediately (for polling readers)
    // 2) stale bridge time events arriving before the backend catches up are
    //    blocked in trigger() instead of reverting the seek bar.
    // Cleared once a bridge event close to the target arrives (±2s).
    let seekTarget: number | null = null;
    let _time = 0;

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
            get currentTime() { return seekTarget ?? _time; },
            set currentTime(v: number) { _time = v; },
            duration: 0,
            addEventListener: (event: string, cb: any) => eventEmitter.addListener(event, cb),
            removeEventListener: (event: string, cb: any) => eventEmitter.removeListener(event, cb),
            on: (event: string, cb: any) => eventEmitter.on(event, cb),
            disableMQADecoder: () => {},
            enableMQADecoder: () => {},
            listDevices: () => {
                sendIpc("player.devices.get");
            },
            load: (url: string, streamFormat: string, encryptionKey: string = "") => {
                seekTarget = null;
                sendIpc("player.load", url, streamFormat, encryptionKey);
            },
            play: () => {
                sendIpc("player.play");
            },
            pause: () => {
                sendIpc("player.pause");
            },
            stop: () => {
                sendIpc("player.stop");
            },
            seek: (time: number) => {
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
        return player;
    }

    return {
        Player,
        // Internal setter for playback controller — updates _time without
        // emitting events or triggering backend seeks.  The currentTime getter
        // (seekTarget ?? _time) ensures the correct value is always returned.
        _setTime: (t: number) => { _time = t; },
        trigger: (event: string, target: any, gen?: number) => {
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }
            if (event === "mediacurrenttime" && activePlayer) {
                // While a seek is pending, block bridge events that are far from
                // the target (stale time from before the seek).  Accept once the
                // backend catches up (within 2s of the target).
                if (seekTarget !== null) {
                    if (Math.abs(target - seekTarget) > 2.0) return;
                    seekTarget = null;
                }
                activePlayer.currentTime = target;
            } else if (event === "mediaduration" && activePlayer) {
                activePlayer.duration = target;
            }
            activeEmitter?.emit?.(event, { target });
        }
    };
}
