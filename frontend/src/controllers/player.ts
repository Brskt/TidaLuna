import { sendIpc } from "../ipc";
import type { AudioDevice } from "../types";

export const createNativePlayerComponent = () => {
    let activeEmitter: any = null;
    let activePlayer: any = null;
    let activeGen = 0;
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
            currentTime: 0,
            duration: 0,
            addEventListener: (event: string, cb: any) => {
                eventEmitter.addListener(event, cb);
            },
            removeEventListener: (event: string, cb: any) => eventEmitter.removeListener(event, cb),
            on: (event: string, cb: any) => eventEmitter.on(event, cb),
            disableMQADecoder: () => {

            },
            enableMQADecoder: () => {
            },
            listDevices: () => {
                sendIpc("player.devices.get");
            },
            load: (url: string, streamFormat: string, encryptionKey: string = "") => {
                sendIpc("player.load", url, streamFormat, encryptionKey);
            },
            play: () => { sendIpc("player.play"); },
            pause: () => { sendIpc("player.pause"); },
            stop: () => { sendIpc("player.stop"); },
            seek: (time: number) => {
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
            releaseDevice: () => {
                console.log("Releasing device");
            },
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
        trigger: (event: string, target: any, gen?: number) => {
            if (gen !== undefined) {
                if (gen < activeGen) return;
                if (gen > activeGen) activeGen = gen;
            }
            event != "mediacurrenttime" && console.debug("NativePlayer:", event, target);
            if (event === "mediacurrenttime" && activePlayer) {
                activePlayer.currentTime = target;
            } else if (event === "mediaduration" && activePlayer) {
                activePlayer.duration = target;
            }
            activeEmitter?.emit?.(event, { target });
        }
    };
}
