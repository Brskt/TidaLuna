// Tests for `applyVolume` (`src/volume.ts`) from its two callers: the native delegate's `setVolume`
// and a proxied element's `volume` write. `followCommittedVolume`, the store road, is not
// installed here.
//
// The forwarder's last level is module state, as Rust's is: every test uses its own levels.

import { beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

let volume = 100;
const storeStub = () => ({
	store: { getState: () => ({ playbackControls: { volume } }) },
});
mock.module("../plugins/lib/src/redux/store", storeStub);

const { adoptRustVolume } = await import("../src/volume");
const { createNativePlayerComponent } = await import("../src/controllers/player");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const volumes = () => sent.filter((s) => s.channel === "player.volume").map((s) => s.args[0]);

// A DASH manifest data URI, which is what `shouldProxy` matches on.
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;

function proxied(id: string) {
	const el = document.createElement("audio");
	el.src = dash(id);
	return el;
}

beforeEach(() => {
	sent.length = 0;
	// Re-asserted per test: `mock.module` is process-global.
	mock.module("../plugins/lib/src/redux/store", storeStub);
});

test("a volume shaka applies to a self-loaded track reaches Rust as the store's level", () => {
	// The keys and the mute button: a saga puts SET_VOLUME, the reducer settles the level, the
	// SDK scales it by ReplayGain and shaka writes the result to the element.
	const el = proxied("keys");
	volume = 40;
	el.volume = 0.33;
	expect(volumes()).toEqual([40]);
});

test("a mute reaches Rust", () => {
	// TOGGLE_MUTE ends on SET_VOLUME {volume: 0}, then on an element write of 0.
	const el = proxied("mute");
	volume = 0;
	el.volume = 0;
	expect(volumes()).toEqual([0]);
});

test("a crossfade ramp and an idle element's silence stay home", () => {
	// Shaka writes the element for things that are not a volume: a ramp every 16 ms, .001 then
	// 0 on the element taking over, 0 on the one going idle. None moves the level.
	const el = proxied("ramp");
	volume = 61;
	el.volume = 0.61;
	sent.length = 0;
	for (const v of [0.001, 0, 0.2, 0.45, 0.61, 0]) el.volume = v;
	expect(volumes()).toEqual([]);
});

test("the write still lands on the element", () => {
	// Shaka reads the outgoing element's volume back to start a crossfade ramp from it.
	const el = proxied("readback");
	volume = 27;
	el.volume = 0.25;
	expect(el.volume).toBe(0.25);
});

test("an element we do not proxy keeps its volume to itself", () => {
	const el = document.createElement("audio");
	volume = 73;
	el.volume = 0.73;
	expect(volumes()).toEqual([]);
});

test("a level Rust reported is not echoed back", () => {
	// Rust's `volume` event re-dispatches SET_VOLUME, and TIDAL applies it to the element.
	const el = proxied("echo");
	adoptRustVolume(52);
	volume = 52;
	el.volume = 0.52;
	expect(volumes()).toEqual([]);
});

test("the native delegate and the element share one road", () => {
	// A native track applied 88; a later element write at the same level is not a change.
	const player = createNativePlayerComponent().Player();
	volume = 88;
	player.setVolume(87.5);
	const el = proxied("shared");
	el.volume = 0.88;
	expect(volumes()).toEqual([88]);
});
