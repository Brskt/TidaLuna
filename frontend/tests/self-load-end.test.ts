// What the bridge does when Rust ends a self-loaded track (src/index.ts, `completed`).
//
// A self-loaded track ends in Rust and the element TIDAL plays never ends by itself. The bridge says
// it on that element, and TIDAL's own end-of-track path runs (the swap to the track it preloaded,
// the repeat modes, the autoplay mix after the last track); this page decides nothing about the
// queue, in any repeat mode.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let repeatMode = 0;
let queueIndex = 0;

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({
			playQueue: {
				currentIndex: queueIndex,
				repeatMode,
				elements: [1, 2, 3].map((id) => ({ mediaItemId: id, uid: `pq_${id}`, context: {} })),
			},
			playbackControls: { mediaProduct: {} },
			content: { mediaItems: {} },
		}),
		dispatch: (a: Dispatch) => void dispatched.push(a),
		subscribe: () => () => {},
	},
}));
bootStubs();

await import("../src/index");
const { setSelfLoad, proxyReset, proxySetDuration, proxySetTime } = await import("../src/audio-proxy");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const settle = () => new Promise((r) => setTimeout(r, 0));
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;

// Two elements, as Shaka keeps: the first manifest is the one playing, the second its preload.
const elements = () => {
	const counts = { first: 0, second: 0 };
	const first = document.createElement("audio");
	const second = document.createElement("audio");
	first.addEventListener("ended", () => counts.first++);
	second.addEventListener("ended", () => counts.second++);
	first.src = dash("playing");
	second.src = dash("preloaded");
	return { first, second, counts };
};

// The queue and playback moves only: `completed` also dispatches the playback state.
const moves = () => dispatched.filter((d) => d.type.startsWith("playQueue/") || d.type.startsWith("player/"));

let realPlayback: unknown;

beforeEach(() => {
	dispatched = [];
	repeatMode = 0;
	queueIndex = 0;
	proxyReset();
	(window as any).NativePlayerComponent = { trigger: () => {} };
	realPlayback = (window as any).nativeInterface.playback;
	(window as any).nativeInterface.playback = { setCurrentMediaItem: () => {}, replaySelfLoad: () => {} };
	setSelfLoad(true);
});

afterEach(() => {
	setSelfLoad(false);
	proxyReset();
	(window as any).nativeInterface.playback = realPlayback;
});

test("the end is told to the element that plays, and no queue move is made, in any repeat mode", async () => {
	for (const mode of [0, 1, 2]) {
		for (const index of [0, 2]) {
			dispatched = [];
			proxyReset();
			repeatMode = mode;
			queueIndex = index;
			const { counts } = elements();
			push([{ t: "state", v: "completed", seq: 5 }]);
			await settle();

			expect({ mode, index, ended: counts.first, moves: moves() }).toEqual({ mode, index, ended: 1, moves: [] });
		}
	}
});

test("the playing element reads as stopped at the start once its track ended", async () => {
	const { first } = elements();
	proxySetDuration(200);
	proxySetTime(200);
	push([{ t: "state", v: "active" }]);
	push([{ t: "state", v: "completed", seq: 5 }]);
	await settle();

	expect([first.paused, first.currentTime, first.duration]).toEqual([true, 0, 0]);
});

test("a cast's track ending says nothing to any element", async () => {
	const { counts } = elements();
	push([{ t: "state", v: "completed", seq: 5, cast: true }]);
	await settle();

	expect(counts).toEqual({ first: 0, second: 0 });
	expect(moves()).toEqual([]);
});

test("the player bar learns the track ended", async () => {
	elements();
	push([{ t: "state", v: "completed", seq: 5 }]);
	await settle();

	expect(dispatched).toContainEqual({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" });
});

test("the SDK's native player hears a self-load's end before the element does, and only once", async () => {
	// `ended` can make the native player the active one (TIDAL's switch to a track it preloaded there),
	// and an active native player takes a `completed` for its own and transitions a second time.
	const order: string[] = [];
	(window as any).NativePlayerComponent = { trigger: (name: string, target: unknown) => order.push(`${name} ${target}`) };
	const { first } = elements();
	first.addEventListener("ended", () => order.push("ended"));
	push([{ t: "state", v: "completed", seq: 5 }]);
	await settle();

	expect(order).toEqual(["mediastate completed", "ended"]);
});

test("a track that is not self-loaded ending leaves the element the next self-loaded track's end needs", async () => {
	// The SDK's native player ended its track and TIDAL handed over to the track Shaka had preloaded:
	// that one is self-loaded, and Shaka never plays its element on that hand-over.
	setSelfLoad(false);
	const { counts } = elements();
	push([{ t: "state", v: "completed", seq: 5 }]);
	setSelfLoad(true);
	push([{ t: "state", v: "completed", seq: 6 }]);
	await settle();

	expect(counts).toEqual({ first: 1, second: 0 });
});

test("after a cast's track ended no element is left to end a later one", async () => {
	const { counts } = elements();
	push([{ t: "state", v: "completed", seq: 5, cast: true }]);
	push([{ t: "state", v: "completed", seq: 6 }]);
	await settle();

	expect(counts).toEqual({ first: 0, second: 0 });
});
