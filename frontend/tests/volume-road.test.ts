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

test("the native delegate and the element share one road", () => {
	// A native track applied 88; a later element write at the same level is not a change. Shaka
	// writes the element for things that are not a volume too (a crossfade ramp every 16 ms, 0 on
	// the element going idle): only the store's level moves Rust's.
	const player = createNativePlayerComponent().Player();
	volume = 88;
	player.setVolume(87.5);
	const el = proxied("shared");
	el.volume = 0.88;
	expect(volumes()).toEqual([88]);
});
