// Tests what the page does with the pause of a stream Rust has already moved past.
//
// An exclusive or ASIO backend reports the pause that a track change owes the previous track
// after the next load, and Rust stamps it with the seq of the load that stream belonged to. The
// page needs that pause (it is the only NOT_PLAYING Redux gets in those modes, and `PlayState`
// closes the previous track's segment on it), but the SDK must not hear it: it is no state of the
// load now running.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => dispatched.push(a),
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

bootStubs();

await import("../src/index");
const { createNativePlayerComponent } = await import("../src/controllers/player");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const playbackStates = () =>
	dispatched.filter((d) => d.type === "playbackControls/SET_PLAYBACK_STATE").map((d) => d.payload);

let heardBySdk: unknown[] = [];

beforeEach(() => {
	dispatched = [];
	heardBySdk = [];
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	component.Player().addEventListener("mediastate", (e: { target: unknown }) => heardBySdk.push(e.target));
});

// Load 2 has announced itself: its position and its `ready` are the newest the SDK has heard.
const loadTwoStarted = [
	{ t: "time", v: 0, seq: 2 },
	{ t: "state", v: "ready", seq: 2 },
];

test("the pause of the previous load still reaches Redux", () => {
	push([...loadTwoStarted, { t: "state", v: "paused", seq: 1 }]);

	expect(playbackStates()).toEqual(["NOT_PLAYING"]);
});

test("the pause of the previous load is not a state of the SDK's current load", () => {
	push([...loadTwoStarted, { t: "state", v: "paused", seq: 1 }]);

	expect(heardBySdk).toEqual(["ready"]);
});

test("a pause of the current load reaches both", () => {
	push([...loadTwoStarted, { t: "state", v: "paused", seq: 2 }]);

	expect(playbackStates()).toEqual(["NOT_PLAYING"]);
	expect(heardBySdk).toEqual(["ready", "paused"]);
});
