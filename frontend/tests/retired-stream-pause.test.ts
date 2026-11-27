// Exclusive and ASIO report the previous track's pause after the next load, stamped with that
// stream's seq. The plugins need it (`PlayState` closes the previous track's segment on it), but
// the SDK must not hear it: it is no state of the load now running.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: () => {},
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

bootStubs();

await import("../src/index");
const { createNativePlayerComponent } = await import("../src/controllers/player");
const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
// What Luna's interceptors (PlayState among them) were told: outside a cast the store is the SDK's.
let playbackStates: unknown[] = [];

let heardBySdk: unknown[] = [];

beforeEach(() => {
	heardBySdk = [];
	playbackStates = [];
	interceptors["playbackControls/SET_PLAYBACK_STATE"] = new Set([(payload: unknown) => void playbackStates.push(payload)]);
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	component.Player().addEventListener("mediastate", (e: { target: unknown }) => heardBySdk.push(e.target));
});

// Load 2 has announced itself: its position and its `ready` are the newest the SDK has heard.
const loadTwoStarted = [
	{ t: "time", v: 0, seq: 2 },
	{ t: "state", v: "ready", seq: 2 },
];

test("the pause of the previous load reaches the plugins but is no state of the current load", () => {
	push([...loadTwoStarted, { t: "state", v: "paused", seq: 1 }]);

	expect(playbackStates).toEqual(["NOT_PLAYING"]);
	expect(heardBySdk).toEqual(["ready"]);
});

test("a pause of the current load reaches both", () => {
	push([...loadTwoStarted, { t: "state", v: "paused", seq: 2 }]);

	expect(playbackStates).toEqual(["NOT_PLAYING"]);
	expect(heardBySdk).toEqual(["ready", "paused"]);
});
