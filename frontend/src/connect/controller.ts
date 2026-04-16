import { sendIpc, invokeIpc } from "../ipc";
import type { MdnsDevice, MediaInfo, QueueLoadRequestData, RepeatMode } from "./types";

export function createTidalConnectController() {
    return {
        initialize(
            serviceType: string,
            queueServerUrl: string,
            contentServerUrl: string,
            authServerUrl: string,
        ) {
            sendIpc(
                "connect.controller.initialize",
                serviceType,
                queueServerUrl,
                contentServerUrl,
                authServerUrl,
            );
        },
        discoverDevices() {
            sendIpc("connect.controller.discover");
        },
        refreshDevices() {
            sendIpc("connect.controller.refresh");
        },
        connectToDevice(device: MdnsDevice): Promise<unknown> {
            return invokeIpc("connect.controller.connect", device);
        },
        disconnect(stopCasting: boolean): Promise<void> {
            return invokeIpc("connect.controller.disconnect", stopCasting);
        },
        setAuthentication(credentials: string, authToken: string, refreshToken: string) {
            sendIpc("connect.controller.set_auth", credentials, authToken, refreshToken);
        },
        loadMedia(rawMediaInfo: MediaInfo): Promise<unknown> {
            return invokeIpc("connect.controller.load_media", rawMediaInfo);
        },
        loadQueue(queueLoadRequestData: QueueLoadRequestData): Promise<unknown> {
            return invokeIpc("connect.controller.load_queue", queueLoadRequestData);
        },
        playNext(): Promise<void> {
            return invokeIpc("connect.controller.play_next");
        },
        playPrevious(): Promise<void> {
            return invokeIpc("connect.controller.play_previous");
        },
        playOrPause(): Promise<void> {
            return invokeIpc("connect.controller.play_or_pause");
        },
        seek(position: number): Promise<void> {
            return invokeIpc("connect.controller.seek", position);
        },
        selectQueueItem(mediaInfo: MediaInfo): Promise<void> {
            return invokeIpc("connect.controller.select_queue_item", mediaInfo);
        },
        refreshQueue(): Promise<void> {
            return invokeIpc("connect.controller.refresh_queue");
        },
        setVolume(level: number): Promise<void> {
            return invokeIpc("connect.controller.set_volume", level);
        },
        setMute(mute: boolean): Promise<void> {
            return invokeIpc("connect.controller.set_mute", mute);
        },
        setRepeatMode(repeatMode: RepeatMode): Promise<void> {
            return invokeIpc("connect.controller.set_repeat", repeatMode);
        },
        setShuffle(shuffle: boolean): Promise<void> {
            return invokeIpc("connect.controller.set_shuffle", shuffle);
        },
        updateQuality(quality: string): Promise<void> {
            return invokeIpc("connect.controller.update_quality", quality);
        },
    };
}
