// Tests the Connect state snapshot of `setupConnectEventListeners` (`src/connect/index.ts`).
//
// Regression: the snapshot runs from the bundle-load script, ahead of the page that mounts TIDAL's
// store. The store the script holds is the lib's proxy, which throws on its first use until the
// store is published, and the snapshot was dropped whole with nothing to say so.

import { beforeEach, expect, mock, spyOn, test } from "bun:test";
import { StoreNotPublishedError } from "../plugins/lib/src/redux/storeNotPublished";

type Action = { type: string; payload?: any };

// What Rust answers `connect.get_state` with; null leaves the call to the real `invokeIpc`.
let answers: Array<() => Promise<unknown>> | null = null;
let asked = 0;
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: (channel: string, ...args: unknown[]) => {
		if (channel !== "connect.get_state" || !answers) return ipc.invokeIpc(channel, ...args);
		return answers[Math.min(asked++, answers.length - 1)]();
	},
}));

const { setupConnectEventListeners } = await import("../src/connect");

// The lib's proxy: any use throws until the store is published.
let published = false;
let log: Action[] = [];
let failWith: Error | null = null;
const store = new Proxy({} as never, {
	get(_, prop) {
		if (!published) throw new StoreNotPublishedError();
		if (prop === "dispatch") {
			return (action: Action) => {
				if (failWith) throw failWith;
				log.push(action);
			};
		}
		return () => ({});
	},
});

const snapshot = (devices: unknown[]) => ({
	devices,
	session: { sessionId: "s1", device: { id: "d1" } },
	media: { id: "m1" },
	playerStatus: { playerState: "PLAYING", progress: 3 },
});
const settled = () => new Promise((r) => setTimeout(r, 0));
const types = () => log.map((a) => a.type);

beforeEach(() => {
	published = false;
	log = [];
	failWith = null;
	asked = 0;
	answers = null;
});

test("the snapshot is read once the store is published, and applied in order", async () => {
	answers = [async () => snapshot([{ id: "d1" }])];
	let publish!: () => void;
	const ready = new Promise<void>((r) => (publish = r));

	const done = setupConnectEventListeners(store, () => ready);
	await settled();
	expect(log).toEqual([]);

	published = true;
	publish();
	await done;

	expect(types()).toEqual([
		"remotePlayback/DEVICES_RECEIVED",
		"remotePlayback/DEVICE_CONNECTED",
		"remotePlayback/tidalConnect/MEDIA_CHANGED",
		"remotePlayback/tidalConnect/UPDATE_PLAYER_STATE",
	]);
});

test("nothing is read from Rust until the store is published, and the snapshot applied is the one read then", async () => {
	const readAt: boolean[] = [];
	answers = [
		async () => {
			readAt.push(published);
			return snapshot([{ id: "old" }, { id: "new" }]);
		},
	];
	let publish!: () => void;
	const ready = new Promise<void>((r) => (publish = r));

	const done = setupConnectEventListeners(store, () => ready);
	await settled();
	expect(asked).toBe(0);

	published = true;
	publish();
	await done;

	expect(readAt).toEqual([true]);
	expect(log.filter((a) => a.type === "remotePlayback/DEVICES_RECEIVED")).toEqual([
		{ type: "remotePlayback/DEVICES_RECEIVED", payload: { deviceType: "tidalConnect", devices: [{ id: "old" }, { id: "new" }] } },
	]);
});

test("a Rust that does not answer is reported and leaves the push events to take over", async () => {
	answers = [() => Promise.reject(new Error("no answer"))];
	published = true;
	const warn = spyOn(console, "warn").mockImplementation(() => {});
	try {
		await setupConnectEventListeners(store, async () => {});
		expect(warn.mock.calls.map((c) => c[0])).toEqual(["[connect] state snapshot unavailable:"]);
		expect(log).toEqual([]);
	} finally {
		warn.mockRestore();
	}
});

test("a store that fails for another reason is reported, not taken for an unpublished one", async () => {
	answers = [async () => snapshot([{ id: "d1" }])];
	published = true;
	failWith = new Error("reducer threw");
	const error = spyOn(console, "error").mockImplementation(() => {});
	try {
		await setupConnectEventListeners(store, () => {
			throw new Error("waited for a store that was there");
		});
		expect(error.mock.calls.map((c) => c[0])).toEqual(["[connect] state snapshot not applied:"]);
		expect(error.mock.calls[0][1]).toBe(failWith);
	} finally {
		error.mockRestore();
	}
});

// A push Rust posted before the snapshot read can reach the renderer behind the snapshot's reply,
// though the reply already shows the state that push led to. Rust stamps its lifecycle events with
// the socket generation they were posted at, and the snapshot's session carries the one it is on.
const emit = (channel: string, payload?: unknown) => (window as any).__LUNAR_IPC_EMIT__(channel, payload);
const device = { id: "d1", type: "tidalConnect" };
const liveAt = (connection: number) => ({ sessionId: "s1", device, joined: true, connection });
const startedAt = (connection: number) => ({ connectedDevice: device, sessionId: "s1", joined: true, connection });
const D3 = "remotePlayback/DEVICE_CONNECTED";
const O3 = "remotePlayback/DEVICE_DISCONNECTED";

// Earlier tests left their relays registered on the shared listener store.
const isolate = () => {
	const listeners = (window as any).__LUNAR_IPC_LISTENERS__;
	for (const channel of Object.keys(listeners)) delete listeners[channel];
};

test("a loss and a resume queued behind a snapshot that shows the resume leave TIDAL connected", async () => {
	isolate();
	answers = [async () => ({ session: liveAt(2) })];
	published = true;
	await setupConnectEventListeners(store, async () => {});

	emit("connect.connection_lost", { connection: 1 });
	emit("connect.session_started", startedAt(2));

	expect(types()).toEqual([D3]);
});

test("a snapshot after a push delivered the start leaves the pushes behind it to carry what followed", async () => {
	isolate();
	let reply!: (state: unknown) => void;
	answers = [() => new Promise((r) => (reply = r))];
	published = true;
	const done = setupConnectEventListeners(store, async () => {});
	await settled();
	emit("connect.session_started", startedAt(1));

	// An end and a start on a newer socket were posted before the read, and are still to come.
	reply({ session: liveAt(3) });
	await done;
	emit("connect.session_ended", { suspended: true, connection: 2 });
	emit("connect.session_started", startedAt(3));

	expect(types()).toEqual([D3, O3, D3]);
	// The push's start, which carries `resumed`; a start replayed from the snapshot does not.
	expect((log[2] as any).payload.resumed).toBe(true);
});
