// Tests the position throttle carve-out in `src/index.ts`.
//
// The renderer throttles `playbackControls/TIME_UPDATE` to one dispatch per 250 ms. That
// window is armed by whatever track was playing; the FIRST position of the next track
// is the one report it drops, and the bar goes on rendering the old track's position
// under the new one until the window elapses. `load()` and `SEEK` each arm a bypass for
// exactly this reason; a crossfade promotion goes through neither.
//
// The batches below are the real shapes Rust produces, which is not the obvious one:
// `ui/flush.rs::take_flush_batch` APPENDS the pending position behind the events already
// queued: a promotion arrives as `[state:completed, time]`, identity first. That
// order is deliberate (the SDK reads the outgoing track's end position synchronously on
// `completed`), and these fixtures must keep it.

import { beforeEach, expect, mock, test } from "bun:test";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { elements: [], currentIndex: 0 } }),
		dispatch: (a: Dispatch) => dispatched.push(a),
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

// `src/index.ts` is the application entry point: importing it drags in the whole Luna
// core. Only the bridge function under test matters here; everything it boots is
// stubbed out. Any import added to index.ts that reaches @luna/core will surface as a
// resolution error in this file, which is the intended tripwire.
mock.module("../render/src", () => ({
	initCore: () => {},
	modules: {},
	defineHostModule: () => {},
	LunaPlugin: class {},
}));
mock.module("../plugins/lib/src", () => ({}));
mock.module("../plugins/lib.native/src/index.native", () => ({}));
mock.module("../src/bootstrap", () => ({}));
// happy-dom ships no `navigator.mediaSession`, which the state arm writes to.
mock.module("../src/controllers/mediasession", () => ({
	setupActionHandlers: () => {},
	updateMetadata: () => {},
	updatePlaybackState: () => {},
}));

await import("../src/index");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const times = () => dispatched.filter((d) => d.type === "playbackControls/TIME_UPDATE");

beforeEach(() => {
	dispatched = [];
	// Re-asserted per test: `mock.module` is process-global and a sibling test file
	// registers this same path with its own dispatch sink.
	mock.module("../plugins/lib/src/redux/store", storeStub);
	(window as any).NativePlayerComponent = { trigger: () => {} };
});

test("a transition's position is dispatched even inside the outgoing track's throttle window", () => {
	// The outgoing track's last periodic report arms the 250 ms window.
	push([{ t: "time", v: 236.4, seq: 1 }]);
	expect(times()).toHaveLength(1);

	// The promotion, in the three batches Rust actually flushes.
	dispatched = [];
	push([
		{ t: "state", v: "completed", seq: 2 },
		{ t: "time", v: 6.0, seq: 2 },
	]);
	push([{ t: "duration", v: 246.0, seq: 2 }]);
	push([{ t: "state", v: "active", seq: 2 }]);

	expect(times()).toEqual([{ type: "playbackControls/TIME_UPDATE", payload: 6.0 }]);
});

test("ordinary periodic reports are still throttled", () => {
	// The counterweight: the carve-out must not disarm the throttle for everything.
	// Two plain position reports back to back are still one dispatch.
	push([{ t: "time", v: 10.0, seq: 1 }]);
	dispatched = [];
	push([{ t: "time", v: 10.25, seq: 1 }]);

	expect(times()).toEqual([]);
});

test("a completion carrying no position does not leave the bypass armed", () => {
	// Exclusive and ASIO output end a track with the state alone, no position beside
	// it. Arming on the state alone would leave the flag set until some unrelated
	// later report spent it, bypassing the throttle for a track that never transitioned.
	push([{ t: "time", v: 20.0, seq: 1 }]);
	dispatched = [];
	push([{ t: "state", v: "completed", seq: 1 }]);
	push([{ t: "time", v: 20.25, seq: 1 }]);

	expect(times()).toEqual([]);
});
