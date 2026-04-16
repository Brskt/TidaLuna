// Public entry point for the `connect` frontend module.
//
// `index.ts` wires the IPC events coming from Rust to Redux dispatches.
// Listeners are kept as thin relays: the single listener that needs to
// call out to the TIDAL API and mutate the DOM lives in `middleware.ts`.

import { invokeIpc, onIpcEvent } from "../ipc";
import { handleReceiverMediaChanged } from "./middleware";
import type {
    ConnectStateSnapshot,
    DevicesReceivedPayload,
    MediaChangedPayload,
    MuteChangedPayload,
    PlayerStatusChangedPayload,
    QueueChangedPayload,
    ReceiverMediaChangedPayload,
    ServerErrorPayload,
    SessionEndedPayload,
    SessionStartedPayload,
    VolumeChangedPayload,
} from "./types";

export { createTidalConnectController } from "./controller";
export { createRemoteDesktopController } from "./receiver";

type Store = {
    dispatch: (action: { type: string; payload?: unknown }) => void;
    getState: () => any;
};

export async function setupConnectEventListeners(store: Store) {
    // ── Controller events → Redux remotePlayback actions ─────────────
    onIpcEvent("connect.devices_received", (payload: DevicesReceivedPayload) => {
        store.dispatch({
            type: "remotePlayback/DEVICES_RECEIVED",
            payload: { deviceType: "tidalConnect", devices: payload.devices },
        });
    });

    onIpcEvent("connect.session_started", (data: SessionStartedPayload) => {
        store.dispatch({
            type: "remotePlayback/DEVICE_CONNECTED",
            payload: {
                device: data.connectedDevice,
                session: { sessionId: data.sessionId },
                resumed: data.joined,
            },
        });
    });

    onIpcEvent("connect.session_ended", (data: SessionEndedPayload) => {
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

    onIpcEvent("connect.media_changed", (data: MediaChangedPayload) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/MEDIA_CHANGED",
            payload: data?.mediaInfo ?? data,
        });
    });

    onIpcEvent("connect.player_status_changed", (data: PlayerStatusChangedPayload) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/UPDATE_PLAYER_STATE",
            payload: {
                playerState: data.playerState,
                progress: data.progress,
            },
        });
    });

    onIpcEvent("connect.queue_changed", (data: QueueChangedPayload) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/QUEUE_CHANGED",
            payload: data?.queueInfo ?? data,
        });
    });

    onIpcEvent("connect.queue_items_changed", (data: QueueChangedPayload) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/QUEUE_ITEMS_CHANGED",
            payload: data?.queueInfo ?? data,
        });
    });

    onIpcEvent("connect.server_error", (data: ServerErrorPayload) => {
        store.dispatch({
            type: "remotePlayback/tidalConnect/HANDLE_ERROR",
            payload: { errorCode: data.errorCode, details: data.details },
        });
    });

    // ── Receiver events ──────────────────────────────────────────────
    onIpcEvent("connect.receiver.session_state", (data: unknown) => {
        store.dispatch({
            type: "remotePlayback/remotePlaybackReceiver/STATE_CHANGED",
            payload: data,
        });
    });

    onIpcEvent("connect.receiver.media_changed", (data: ReceiverMediaChangedPayload) => {
        // Enrichment + DOM mutation lives in the middleware so the listener
        // stays a pure relay.
        void handleReceiverMediaChanged(store, data);
    });

    onIpcEvent("connect.receiver.volume_changed", (data: VolumeChangedPayload) => {
        if (typeof data?.volume === "number") {
            store.dispatch({
                type: "playbackControls/SET_VOLUME",
                payload: { volume: Math.round(data.volume) },
            });
        }
    });

    onIpcEvent("connect.receiver.mute_changed", (data: MuteChangedPayload) => {
        if (typeof data?.mute === "boolean") {
            store.dispatch({
                type: "playbackControls/SET_MUTE",
                payload: data.mute,
            });
        }
    });

    // ── Bootstrap snapshot: fetch current state on setup ─────────────
    try {
        const state = (await invokeIpc("connect.get_state")) as ConnectStateSnapshot;
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
        // Connect not yet initialized - push events will take over.
    }
}
