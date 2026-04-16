import { sendIpc } from "../ipc";
import type { RepeatMode } from "./types";

export function createRemoteDesktopController() {
    return {
        initialize() {
            sendIpc("connect.receiver.start", {});
        },
        startBroadcasting(userId: string) {
            sendIpc("connect.receiver.start", { userId });
        },
        stopBroadcasting() {
            sendIpc("connect.receiver.stop");
        },
        // Status / progress / completion events are generated from the player
        // in Rust (`src/connect/bridge.rs::forward`) and do not need a
        // frontend round-trip; the TIDAL SDK calls these into the void.
        statusUpdated(_status: unknown) {},
        progressUpdated(_progress: number) {},
        playbackCompleted(_hasNextMedia: boolean) {},
        requestNextMedia() {},
        disconnect() {
            sendIpc("connect.receiver.stop");
        },
        setRepeatMode(repeatMode: RepeatMode) {
            sendIpc("connect.receiver.set_repeat", repeatMode);
        },
        setShuffle(shuffle: boolean) {
            sendIpc("connect.receiver.set_shuffle", shuffle);
        },
    };
}
