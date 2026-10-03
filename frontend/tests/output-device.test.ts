// Tests for `src/output-device.ts`.
//
// The store below is shaped like TIDAL's `player` slice, `availableDevices` being the SDK's
// `OutputDevice`s as `SET_AVAILABLE_DEVICES` clones them. A `commit` of a device stands for
// `player/SET_ACTIVE_DEVICE_SUCCESS`, the action the SDK commits a switch with.

import { afterEach, beforeEach, expect, test } from "bun:test";
import type { SentIpc } from "./setup";

const { followCommittedOutput } = await import("../src/output-device");
const { setSelfLoad } = await import("../src/audio-proxy");
const { createNativePlayerComponent } = await import("../src/controllers/player");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const deviceSets = () => sent.filter((s) => s.channel === "player.devices.set").map((s) => s.args);
const W = window as any;

const devices = [
	{ id: "default", name: "System Default", nativeDeviceId: "default", webDeviceId: "default" },
	{ id: "guid-a", name: "Speakers (A)", nativeDeviceId: "Speakers (A)", webDeviceId: "web-a" },
	{ id: "guid-b", name: "Headphones (B)", nativeDeviceId: "Headphones (B)", webDeviceId: "web-b" },
	// Listed by Chromium only: `mergeDevices` found no Rust device of that name.
	{ id: "guid-c", name: "Monitor (C)", webDeviceId: "web-c" },
];

function fakeStore(activeDeviceId: string, activeDeviceMode = "shared") {
	let state = { player: { activeDeviceId, activeDeviceMode, availableDevices: devices } };
	const listeners = new Set<() => void>();
	return {
		getState: () => state,
		subscribe(cb: () => void) {
			listeners.add(cb);
			return () => listeners.delete(cb);
		},
		// A reducer ran: the new `player` slice, or the same one for an unrelated action.
		commit(player: Partial<typeof state.player> = {}) {
			state = { player: { ...state.player, ...player } };
			for (const cb of listeners) cb();
		},
	};
}

let unsubscribe: (() => void) | undefined;
function follow(store: ReturnType<typeof fakeStore>) {
	unsubscribe = followCommittedOutput(store);
}

beforeEach(() => {
	sent.length = 0;
	W.__TIDALUNAR_ASIO__ = false;
	setSelfLoad(true);
});

afterEach(() => {
	unsubscribe?.();
	unsubscribe = undefined;
	setSelfLoad(false);
});

test("a device TIDAL switches to during a self-loaded track reaches Rust", () => {
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "guid-b", activeDeviceMode: "shared" });
	expect(deviceSets()).toEqual([["Headphones (B)", "shared"]]);
});

test("the system default reaches Rust under the id the delegate sends it by", () => {
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "default" });
	expect(deviceSets()).toEqual([["auto", "shared"]]);
});

test("leaving exclusive on A for the system default reaches Rust shared", () => {
	// The switch resets TIDAL's mode to shared.
	const store = fakeStore("guid-a", "exclusive");
	follow(store);
	store.commit({ activeDeviceId: "default", activeDeviceMode: "shared" });
	expect(deviceSets()).toEqual([["auto", "shared"]]);
});

test("an exclusive mode TIDAL commits on a chosen device reaches Rust", () => {
	const store = fakeStore("guid-b");
	follow(store);
	store.commit({ activeDeviceMode: "exclusive" });
	expect(deviceSets()).toEqual([["Headphones (B)", "exclusive"]]);
});

test("an exclusive mode TIDAL commits on the system default reaches Rust shared", () => {
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "default", activeDeviceMode: "exclusive" });
	expect(deviceSets()).toEqual([["auto", "shared"]]);
});

test("ASIO stays on across a device switch", () => {
	W.__TIDALUNAR_ASIO__ = true;
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "guid-b" });
	expect(deviceSets()).toEqual([["Headphones (B)", "asio"]]);
});

test("a device only Chromium lists sends Rust nothing", () => {
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "guid-c" });
	expect(deviceSets()).toEqual([]);
});

test("the device at install and every unrelated action send nothing", () => {
	const store = fakeStore("guid-b");
	follow(store);
	for (let i = 0; i < 5; i++) store.commit();
	expect(deviceSets()).toEqual([]);
});

test("a native track is left to the delegate", () => {
	setSelfLoad(false);
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "guid-b" });
	expect(deviceSets()).toEqual([]);
	setSelfLoad(true);
	store.commit();
	expect(deviceSets()).toEqual([]);
});

test("the delegate and a committed switch send Rust the same thing", () => {
	// Rust's no-op for a re-assert of its open device compares these ids and modes.
	const player = createNativePlayerComponent().Player();
	player.selectDevice({ id: "Headphones (B)", name: "Headphones (B)", controllableVolume: true }, "shared");
	const store = fakeStore("guid-a");
	follow(store);
	store.commit({ activeDeviceId: "guid-b" });
	const [fromDelegate, fromStore] = deviceSets();
	expect(fromStore).toEqual(fromDelegate);
});

test("exclusive still wins over ASIO on the delegate", () => {
	W.__TIDALUNAR_ASIO__ = true;
	const player = createNativePlayerComponent().Player();
	player.selectDevice({ id: "Headphones (B)", name: "Headphones (B)", controllableVolume: true }, "exclusive");
	expect(deviceSets()).toEqual([["Headphones (B)", "exclusive"]]);
	expect(W.__TIDALUNAR_ASIO__).toBe(false);
	expect(sent.filter((s) => s.channel === "settings.asio").map((s) => s.args)).toEqual([[false]]);
});
