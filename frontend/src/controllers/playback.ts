import { sendIpc } from "../ipc";

export const createPlaybackController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => {
            delegate = d;
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
        },
        setRepeatMode: (mode: any) => { },
        setShuffle: (shuffle: any) => { },
    }
}
