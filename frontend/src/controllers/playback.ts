import { sendIpc } from "../ipc";

export const createPlaybackController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => { delegate = d; },
        sendPlayerCommand: (cmd: any) => { },
        setCurrentMediaItem: (item: any) => {
            if (item && typeof item === "object") {
                // Send raw media item; metadata parsing/fallbacks are centralized in Rust.
                sendIpc("player.metadata", item);
            }
        },
        // Tidal calls this in multiple internal flows (including resets/progress updates),
        // so forwarding it directly to backend seek causes unwanted jumps (e.g. back to 0).
        setCurrentTime: (time: any) => { },
        setPlayQueueState: (state: any) => { },
        setPlayingStatus: (status: any) => { },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}
