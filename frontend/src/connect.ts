import { sendIpc, invokeIpc, onIpcEvent, tidalFetch } from "./ipc";

// ── nativeInterface.tidalConnect ─────────────────────────────────────

export function createTidalConnectController() {
    return {
        initialize(serviceType: string, queueServerUrl: string, contentServerUrl: string, authServerUrl: string) {
            sendIpc("connect.controller.initialize", serviceType, queueServerUrl, contentServerUrl, authServerUrl);
        },
        discoverDevices() {
            sendIpc("connect.controller.discover");
        },
        refreshDevices() {
            sendIpc("connect.controller.refresh");
        },
        connectToDevice(device: any): Promise<any> {
            return invokeIpc("connect.controller.connect", device);
        },
        disconnect(stopCasting: boolean): Promise<void> {
            return invokeIpc("connect.controller.disconnect", stopCasting);
        },
        setAuthentication(credentials: string, authToken: string, refreshToken: string) {
            sendIpc("connect.controller.set_auth", credentials, authToken, refreshToken);
        },
        loadMedia(rawMediaInfo: any): Promise<any> {
            return invokeIpc("connect.controller.load_media", rawMediaInfo);
        },
        loadQueue(queueLoadRequestData: any): Promise<any> {
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
        selectQueueItem(mediaInfo: any): Promise<void> {
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
        setRepeatMode(repeatMode: string): Promise<void> {
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

// ── nativeInterface.remoteDesktop ────────────────────────────────────

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
        statusUpdated(_status: any) {
            // Handled by flush.rs hook - no IPC needed
        },
        progressUpdated(_progress: number) {
            // Handled by flush.rs hook - no IPC needed
        },
        playbackCompleted(_hasNextMedia: boolean) {
            // Handled by flush.rs hook
        },
        requestNextMedia() {
            // Handled by flush.rs hook
        },
        disconnect() {
            sendIpc("connect.receiver.stop");
        },
        setRepeatMode(repeatMode: string) {
            sendIpc("connect.receiver.set_repeat", repeatMode);
        },
        setShuffle(shuffle: boolean) {
            sendIpc("connect.receiver.set_shuffle", shuffle);
        },
    };
}

// ── Event listeners: Rust → Redux ────────────────────────────────────

export async function setupConnectEventListeners(store: any) {
    // Controller events → Redux remotePlayback actions
    onIpcEvent("connect.devices_received", (devices: any[]) => {
        store.dispatch({
            type: "remotePlayback/DEVICES_RECEIVED",
            payload: { deviceType: "tidalConnect", devices },
        });
    });

    onIpcEvent("connect.session_started", (data: any) => {
        store.dispatch({
            type: "remotePlayback/DEVICE_CONNECTED",
            payload: {
                device: data.connectedDevice,
                session: { sessionId: data.sessionId },
                resumed: data.joined,
            },
        });
    });

    onIpcEvent("connect.session_ended", (data: any) => {
        store.dispatch({
            type: "remotePlayback/DEVICE_DISCONNECTED",
            payload: {
                deviceType: "tidalConnect",
                suspended: data?.suspended,
            },
        });
    });

    onIpcEvent("connect.connection_lost", () => {
        store.dispatch({ type: "remotePlayback/CONNECTION_LOST" });
    });

    onIpcEvent("connect.media_changed", (data: any) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/MEDIA_CHANGED",
            payload: data?.mediaInfo ?? data,
        });
    });

    onIpcEvent("connect.player_status_changed", (data: any) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/UPDATE_PLAYER_STATE",
            payload: {
                playerState: data.playerState,
                progress: data.progress,
            },
        });
    });

    onIpcEvent("connect.queue_changed", (data: any) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/QUEUE_CHANGED",
            payload: data?.queueInfo ?? data,
        });
    });

    onIpcEvent("connect.queue_items_changed", (data: any) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/QUEUE_ITEMS_CHANGED",
            payload: data?.queueInfo ?? data,
        });
    });

    onIpcEvent("connect.server_error", (data: any) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/HANDLE_ERROR",
            payload: { errorCode: data.errorCode, details: data.details },
        });
    });

    // Receiver events
    onIpcEvent("connect.receiver.session_state", (data: any) => {
        store.dispatch({
            type: "remotePlayback/remotePlaybackReceiver/STATE_CHANGED",
            payload: data,
        });
    });

    onIpcEvent("connect.receiver.media_changed", (data: any) => {
        // Fetch full track from TIDAL API, then feed it through the same
        // setCurrentMediaItem path that normal playback uses.
        const trackId = data?.mediaId;
        if (!trackId) return;

        (async () => {
            try {
                const cid = store.getState()?.session?.clientId;
                const cc = store.getState()?.session?.countryCode ?? "US";
                const lang = store.getState()?.settings?.language ?? "en_US";
                const url = `https://desktop.tidal.com/v1/tracks/${trackId}?countryCode=${cc}&deviceType=DESKTOP&locale=${lang}`;
                const res = await tidalFetch(url, cid ? { headers: { "x-tidal-token": cid } } : undefined);

                if (res.status >= 200 && res.status < 300) {
                    const track = await res.json();
                    const cover = track?.album?.cover;
                    const imageUrl = cover
                        ? `https://resources.tidal.com/images/${cover.replace(/-/g, "/")}/${1280}x${1280}.jpg`
                        : undefined;

                    // Inject into content cache so selectors can find it
                    store.dispatch({
                        type: "content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS",
                        payload: { mediaItem: { type: "track", item: track } },
                    });

                    // Update Redux state (player bar React components read from here)
                    const controls = store.getState().playbackControls ?? {};
                    const oldMp = controls.mediaProduct ?? {};
                    store.dispatch({
                        type: "playbackControls/MEDIA_PRODUCT_TRANSITION",
                        payload: {
                            mediaProduct: { ...oldMp, productId: String(trackId), productType: "track" },
                            playbackContext: { ...controls.playbackContext, actualProductId: String(trackId), actualDuration: track.duration ?? (data?.metadata?.duration ?? 0) / 1000, actualAudioQuality: track.audioQuality ?? "LOSSLESS" },
                        },
                    });

                    // Feed through the normal playback path (MediaSession, Rust metadata, interceptors)
                    const item = {
                        id: track.id,
                        productId: track.id,
                        title: track.title,
                        artist: track.artists?.[0]?.name ?? track.artist?.name ?? "",
                        album: track.album?.title ?? "",
                        imageUrl,
                        duration: track.duration,
                        type: "track",
                    };
                    (window as any).nativeInterface?.playback?.setCurrentMediaItem?.(item);

                    // Set cover via background-image CSS (doesn't interfere with React's DOM).
                    // TIDAL's SDK manages its own cover <img> but doesn't render it for
                    // externally-driven playback (Connect receiver).
                    if (cover && /^[0-9a-f-]+$/i.test(cover)) {
                        const thumbUrl = `https://resources.tidal.com/images/${cover.replace(/-/g, "/")}/80x80.jpg`;
                        const fig = document.querySelector('[data-test="current-media-imagery"]') as HTMLElement | null;
                        if (fig) {
                            fig.style.backgroundImage = `url("${thumbUrl}")`;
                            fig.style.backgroundSize = "cover";
                            fig.style.backgroundPosition = "center";
                            fig.style.borderRadius = "4px";
                        }
                    }
                }
            } catch {
                // API fetch failed - non-critical
            }
        })();
    });

    // Volume/mute sync from Connect receiver -> player bar slider
    onIpcEvent("connect.receiver.volume_changed", (data: any) => {
        if (typeof data?.volume === "number") {
            store.dispatch({
                type: "playbackControls/SET_VOLUME",
                payload: { volume: Math.round(data.volume) },
            });
        }
    });

    onIpcEvent("connect.receiver.mute_changed", (data: any) => {
        if (typeof data?.mute === "boolean") {
            store.dispatch({
                type: "playbackControls/SET_MUTE",
                payload: data.mute,
            });
        }
    });

    // Bootstrap snapshot: fetch current state on setup
    try {
        const state = await invokeIpc("connect.get_state");
        if (state?.devices?.length) {
            store.dispatch({
                type: "remotePlayback/DEVICES_RECEIVED",
                payload: { deviceType: "tidalConnect", devices: state.devices },
            });
        }
        if (state?.session) {
            store.dispatch({
                type: "remotePlayback/DEVICE_CONNECTED",
                payload: { device: state.session.device, session: state.session },
            });
        }
        if (state?.media) {
            store.dispatch({
                type: "remotePlayback/tidalConnect/MEDIA_CHANGED",
                payload: state.media,
            });
        }
        if (state?.playerStatus) {
            store.dispatch({
                type: "remotePlayback/tidalConnect/UPDATE_PLAYER_STATE",
                payload: state.playerStatus,
            });
        }
    } catch {
        // Connect not yet initialized - push events will take over
    }
}
