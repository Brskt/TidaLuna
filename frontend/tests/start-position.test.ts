// Tests the position Rust announces as a load starts (`starttime`, src/player/thread/load.rs). It
// is the page's to show, never the SDK's to adopt: its `currentTime` holds the position its
// own load() was given until that load resolves, and reads it to restore a track at launch.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Action = { type: string; payload?: unknown };

let timeDispatches: unknown[] = [];

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Action) => {
			if (a.type === "playbackControls/TIME_UPDATE") timeDispatches.push(a.payload);
		},
		subscribe: () => () => {},
	},
}));

bootStubs();

await import("../src/index");
const { createNativePlayerComponent } = await import("../src/controllers/player");

let sdkHeard: unknown[] = [];
let player: { currentTime: number };

beforeEach(() => {
	timeDispatches = [];
	sdkHeard = [];
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	player = component.Player();
	(player as any).addEventListener("mediacurrenttime", (e: { target: unknown }) => void sdkHeard.push(e.target));
	(window as any).__LUNAR_FORCE_TIME_DISPATCH__();
});

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);

test("a load's start position moves the page's position and never the SDK's, which hears the next ones", () => {
	push([{ t: "starttime", v: 42, seq: 1 }]);

	expect(timeDispatches).toEqual([42]);
	expect(player.currentTime).toBe(42);
	expect(sdkHeard).toEqual([]);

	push([{ t: "time", v: 43, seq: 1 }]);
	expect(sdkHeard).toEqual([43]);
});
