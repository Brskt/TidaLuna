// Tests the Connect state snapshot of `setupConnectEventListeners` (`src/connect/index.ts`).
//
// Regression: the snapshot runs from the bundle-load script, ahead of the page that mounts TIDAL's
// store. The store the script holds is the lib's proxy, which throws on its first use until the
// store is published, and the snapshot was dropped whole with nothing to say so.

import { afterEach, beforeEach, expect, mock, spyOn, test } from "bun:test";
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

// `setupConnectEventListeners` binds relays on the page-wide bus; later files must not find them.
const bus = (window as any).__LUNAR_IPC_LISTENERS__;
let busBefore: Record<string, unknown[]>;

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
	busBefore = Object.fromEntries(Object.entries(bus).map(([k, v]) => [k, [...(v as unknown[])]]));
});
afterEach(() => {
	answers = null;
	for (const k of Object.keys(bus)) {
		if (busBefore[k]) bus[k] = busBefore[k];
		else delete bus[k];
	}
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
