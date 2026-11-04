// Public entry point for the `connect` frontend module.
//
// `index.ts` wires the IPC events coming from Rust to Redux dispatches.
// Listeners are kept as thin relays: the single listener that needs to
// call out to the TIDAL API and mutate the DOM lives in `middleware.ts`.

import { invokeIpc, onIpcEvent } from "../ipc";
import { isStoreNotPublished } from "../../plugins/lib/src/redux/storeNotPublished";
import { storePublished } from "../store-published";
import { isOutputLevelReport, mirrorRustLevel } from "../volume";
import { handleReceiverMediaChanged } from "./middleware";
import type {
    ConnectionLostPayload,
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

export async function setupConnectEventListeners(store: Store, published: () => Promise<void> = storePublished) {
    onIpcEvent("connect.devices_received", (payload: DevicesReceivedPayload) => {
        store.dispatch({
            type: "remotePlayback/DEVICES_RECEIVED",
            payload: { deviceType: "tidalConnect", devices: payload.devices },
        });
    });

    // Rust stamps every lifecycle event with the socket generation it was posted at. A push queued
    // before the snapshot read lands behind the snapshot's reply; one older than a generation this
    // page applied is stale, and applying it would undo what the page already replaced.
    let newest = 0;
    const admit = (connection: number | undefined) => {
        if (connection === undefined) return true;
        if (connection < newest) return false;
        newest = connection;
        return true;
    };

    // TIDAL forks its cast-listening saga on every DEVICE_CONNECTED, and only DEVICE_DISCONNECTED
    // cancels it: CONNECTION_LOST leaves it running. A connect finding one armed ends it first; a
    // start repeated for the armed connection is dropped, as answering it would reset the player.
    let armedFor: string | undefined;
    let lostSinceArmed = false;
    const dispatchConnected = (
        connection: number | undefined,
        payload: { device: unknown; session: { sessionId: string }; resumed?: boolean },
    ) => {
        const identity = `${connection}:${payload.session.sessionId}`;
        if (identity === armedFor) return;
        if (armedFor !== undefined) {
            // After a loss the end returns TIDAL to its own player, which plays when the desired state
            // is PLAYING: TIDAL's reducer-only reset comes first. Absent a loss it stays untouched:
            // the remote's status only restores it when it reports a change.
            if (lostSinceArmed) store.dispatch({ type: "playbackControls/SET_DESIRED_PAUSE_STATE" });
            store.dispatch({
                type: "remotePlayback/DEVICE_DISCONNECTED",
                payload: { deviceType: "tidalConnect", suspended: true },
            });
        }
        store.dispatch({ type: "remotePlayback/DEVICE_CONNECTED", payload });
        armedFor = identity;
        lostSinceArmed = false;
    };

    // A start a push already delivered is not replayed from the snapshot: the pushes behind it,
    // which Rust posted before the snapshot was read, carry every later state in order.
    const snapshotConnected = (session: NonNullable<ConnectStateSnapshot["session"]>) => {
        if (armedFor !== undefined || !admit(session.connection)) return;
        dispatchConnected(session.connection, { device: session.device, session });
    };

    onIpcEvent("connect.session_started", (data: SessionStartedPayload) => {
        if (!admit(data.connection)) return;
        dispatchConnected(data.connection, {
            device: data.connectedDevice,
            session: { sessionId: data.sessionId },
            resumed: data.joined,
        });
    });

    onIpcEvent("connect.session_ended", (data: SessionEndedPayload) => {
        if (!admit(data?.connection)) return;
        armedFor = undefined;
        store.dispatch({
            type: "remotePlayback/DEVICE_DISCONNECTED",
            payload: {
                deviceType: "tidalConnect",
                suspended: data?.suspended,
            },
        });
    });

    // What TIDAL's native layer reports through `TCnotifyConnectFailed`: it releases the queue
    // restore waiting on the device and drops the device TIDAL persisted.
    onIpcEvent("connect.connect_failed", () => {
        store.dispatch({ type: "remotePlayback/CONNECT_TO_DEVICE_FAILED" });
    });

    // The loss stays CONNECTION_LOST alone: TIDAL keeps the persisted session for a resume and its
    // cast listener armed, which the next connect ends (above) once Rust starts a session again.
    onIpcEvent("connect.connection_lost", (data?: ConnectionLostPayload) => {
        if (!admit(data?.connection)) return;
        lostSinceArmed = true;
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
        void handleReceiverMediaChanged(store, data, published);
    });

    // Rust applies a controller's volume and mute itself: the receiver plays with or without a
    // page. These relays only mirror it into TIDAL's store, adopted first (`mirrorRustLevel`) and
    // never echoed back. A move no store saw is caught up at page load (`resyncOutputLevel`).
    for (const channel of ["connect.receiver.volume_changed", "connect.receiver.mute_changed"]) {
        onIpcEvent(channel, (report: unknown) => {
            if (isOutputLevelReport(report)) mirrorRustLevel(store, report);
        });
    }

    // Run from the bundle-load script, ahead of the page that mounts TIDAL's store. The read waits for
    // the store; one taken earlier predates the pushes that reached no store meanwhile, and a snapshot
    // with nothing in it raises no error to say so.
    try {
        try {
            store.getState();
        } catch (e) {
            if (!isStoreNotPublished(e)) throw e;
            await published();
        }
        const state = await readStateSnapshot();
        if (state) applyStateSnapshot(store, state, snapshotConnected);
    } catch (e) {
        console.error("[connect] state snapshot not applied:", e);
    }
}

async function readStateSnapshot(): Promise<ConnectStateSnapshot | undefined> {
    try {
        return (await invokeIpc("connect.get_state")) as ConnectStateSnapshot;
    } catch (e) {
        // Rust did not answer: the push events take over.
        console.warn("[connect] state snapshot unavailable:", e);
        return undefined;
    }
}

function applyStateSnapshot(
    store: Store,
    state: ConnectStateSnapshot,
    connected: (session: NonNullable<ConnectStateSnapshot["session"]>) => void,
): void {
    if (state.devices?.length) {
        store.dispatch({
            type: "remotePlayback/DEVICES_RECEIVED",
            payload: { deviceType: "tidalConnect", devices: state.devices },
        });
    }
    if (state.session) connected(state.session);
    if (state.media) {
        store.dispatch({
            type: "remotePlayback/tidalConnect/MEDIA_CHANGED",
            payload: state.media,
        });
    }
    if (state.playerStatus) {
        store.dispatch({
            type: "remotePlayback/tidalConnect/UPDATE_PLAYER_STATE",
            payload: state.playerStatus,
        });
    }
}
