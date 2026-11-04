// TIDAL forks its cast saga on every DEVICE_CONNECTED and only DEVICE_DISCONNECTED cancels it: a
// connect over a lost connection stacks a second listener, and every cast action fires twice. The
// disconnect also reloads the track locally and plays it when desired reads PLAYING.

import { expect, test } from "bun:test";
import { setupConnectEventListeners } from "../src/connect";

type Action = { type: string };

const types = (actions: Action[]) => actions.map((a) => a.type);

const D3 = "remotePlayback/DEVICE_CONNECTED";
const O3 = "remotePlayback/DEVICE_DISCONNECTED";
const k3 = "remotePlayback/CONNECTION_LOST";
const RESET = "playbackControls/SET_DESIRED_PAUSE_STATE";

const started = (id: string, connection: number) => ({
	connectedDevice: { id, type: "tidalConnect" },
	sessionId: `s-${id}`,
	joined: false,
	connection,
});

// Modelled from the bundle: the reducer (PLAYING or STALLED sets desired to PLAYING, RESET to
// NOT_PLAYING) and the disconnect's reload (`Sft`), a step. The remote's status goes through `g5`,
// which dispatches a state only when it differs from the current one.
function relay() {
	const dispatched: Action[] = [];
	const model = { desired: "NOT_PLAYING", playbackState: "IDLE", localPlays: 0 };
	let loadsPending = 0;

	const setState = (state: string) => {
		model.playbackState = state;
		if (state === "PLAYING" || state === "STALLED") model.desired = "PLAYING";
	};
	const dispatch = (action: Action) => {
		dispatched.push(action);
		if (action.type === RESET) model.desired = "NOT_PLAYING";
		if (action.type === k3) setState("IDLE");
		if (action.type === O3 && model.desired === "PLAYING") loadsPending++;
	};
	// The listeners register before the snapshot read, which this test leaves unanswered.
	void setupConnectEventListeners({ dispatch, getState: () => ({}) });
	const emit = (channel: string, payload?: unknown) => (window as any).__LUNAR_IPC_EMIT__(channel, payload);

	return {
		dispatched,
		model,
		emit,
		remotePlays: () => {
			if (model.playbackState !== "PLAYING") setState("PLAYING");
		},
		// The mini-player offers play, not pause, while desired is not PLAYING.
		offersPlay: () => model.desired !== "PLAYING",
		// `Sft` plays after its load, which finishes behind whatever was dispatched meanwhile.
		loadsFinish: () => {
			model.localPlays += loadsPending;
			loadsPending = 0;
		},
	};
}

test("a resume right after a loss, while TIDAL still wants to play, ends the old listener without a local play", () => {
	const t = relay();
	t.emit("connect.session_started", started("d1", 1));
	t.remotePlays();
	t.emit("connect.connection_lost", { connection: 1 });

	// The loss alone ends nothing.
	expect(types(t.dispatched)).toEqual([D3, k3]);

	t.emit("connect.session_started", { ...started("d1", 2), joined: true });

	t.loadsFinish();
	expect(t.model.localPlays).toBe(0);
	expect(types(t.dispatched)).toEqual([D3, k3, RESET, O3, D3]);

	// The loss left the state at IDLE; the remote's status is then a change and restores desired.
	t.remotePlays();
	expect(t.offersPlay()).toBe(false);
});

// No loss precedes it. A reset here is not restored by the remote's status (the state already
// reads PLAYING), and the next press would pause the device.
test("a connect over an armed connection that no loss preceded leaves the desired state alone", () => {
	for (const ended of [true, false]) {
		const t = relay();
		t.emit("connect.session_started", started("d1", 1));
		t.remotePlays();
		// What Rust sends when a connect replaces a live session: the end, then the start.
		if (ended) t.emit("connect.session_ended", { suspended: true, connection: 2 });
		t.emit("connect.session_started", { ...started("d1", 2), joined: true });
		t.remotePlays();

		expect(types(t.dispatched)).toEqual([D3, O3, D3]);
		expect(t.offersPlay()).toBe(false);
	}
});
