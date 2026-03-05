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
        // Update the player's internal time for immediate UI feedback.
        // No backend seek is triggered — player.seek() handles that separately.
        setCurrentTime: (time: any) => {
            if (typeof time === 'number' && isFinite(time) && time >= 0) {
                (window.NativePlayerComponent as any)?._setTime?.(time);
            }
        },
        setPlayQueueState: (state: any) => { },
        setPlayingStatus: (status: any) => { },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}
