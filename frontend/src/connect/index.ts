// Public entry point for the `connect` frontend module.
//
// `index.ts` wires the IPC events coming from Rust to Redux dispatches.
// Listeners are kept as thin relays: the single listener that needs to
// call out to the TIDAL API and mutate the DOM lives in `middleware.ts`.

import { invokeIpc, onIpcEvent } from "../ipc";
import { isOutputLevelReport, mirrorRustLevel } from "../volume";
import { handleReceiverMediaChanged } from "./middleware";
import type {
    ConnectStateSnapshot,
    DevicesReceivedPayload,
    MediaChangedPayload,
    PlayerStatusChangedPayload,
    QueueChangedPayload,
    ReceiverMediaChangedPayload,
    ServerErrorPayload,
    SessionEndedPayload,
    SessionStartedPayload,
} from "./types";

export { createTidalConnectController } from "./controller";
export { createRemoteDesktopController } from "./receiver";

type Store = {
    dispatch: (action: { type: string; payload?: unknown }) => void;
    getState: () => any;
};

export async function setupConnectEventListeners(store: Store) {
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

    onIpcEvent("connect.receiver.media_changed", (data: ReceiverMediaChangedPayload) => {
        // Enrichment + DOM mutation lives in the middleware: the listener
        // stays a pure relay.
        void handleReceiverMediaChanged(store, data);
    });

    // Rust applies a controller's volume and mute itself: the receiver plays with or without a
    // page. These relays only mirror it into TIDAL's store, adopted first (`mirrorRustLevel`) and
    // never echoed back. A move no store saw is caught up at page load (`resyncOutputLevel`).
    for (const channel of ["connect.receiver.volume_changed", "connect.receiver.mute_changed"]) {
        onIpcEvent(channel, (report: unknown) => {
            if (isOutputLevelReport(report)) mirrorRustLevel(store, report);
        });
    }

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
