// Tests how the bridge in `src/index.ts` hands Rust's transport state on: outside a cast TIDAL's
// store is left alone, during one the bridge writes it. Luna's interceptors (PlayState,
// DiscordRPC) never see the SDK's write: they get ours, once, either way.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Action = { type: string; payload?: unknown };

const state: { playbackControls: { playbackState?: string }; playQueue: object } = {
	playbackControls: {},
	playQueue: { currentIndex: 0, elements: [] },
};
let dispatched: unknown[] = [];

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => state,
		dispatch: (a: Action) => {
			if (a.type !== "playbackControls/SET_PLAYBACK_STATE") return;
			dispatched.push(a.payload);
			state.playbackControls.playbackState = a.payload as string;
		},
		subscribe: () => () => {},
	},
}));

bootStubs();

await import("../src/index");
const { createNativePlayerComponent } = await import("../src/controllers/player");
const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");

const SDK_STATE: Record<string, string> = {
	active: "PLAYING",
	paused: "NOT_PLAYING",
	stopped: "NOT_PLAYING",
};

let delivered: unknown[] = [];
// Whether the stand-in SDK writes the state it hears, as TIDAL's feed does synchronously.
let sdkWrites = true;

beforeEach(() => {
	dispatched = [];
	delivered = [];
	sdkWrites = true;
	state.playbackControls.playbackState = "PLAYING";
	interceptors["playbackControls/SET_PLAYBACK_STATE"] = new Set([(payload: unknown) => void delivered.push(payload)]);
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	component.Player().addEventListener("mediastate", (e: { target: string }) => {
		const value = SDK_STATE[e.target];
		if (sdkWrites && value) state.playbackControls.playbackState = value;
	});
});

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);

test("a pause the SDK writes itself reaches the plugins and leaves the store alone", () => {
	push([{ t: "state", v: "paused", seq: 1 }]);

	expect(dispatched).toEqual([]);
	expect(delivered).toEqual(["NOT_PLAYING"]);
	expect(state.playbackControls.playbackState).toBe("NOT_PLAYING");
});

test("a cast's play the store already holds is not written again: a repeated PLAYING drops counted time", () => {
	sdkWrites = false;
	push([{ t: "state", v: "active", seq: 1, cast: true }]);

	expect(dispatched).toEqual([]);
	expect(delivered).toEqual(["PLAYING"]);
});

test("a late pause after a reset leaves its IDLE, which makes the next Play reload", () => {
	sdkWrites = false;
	state.playbackControls.playbackState = "IDLE";
	push([
		{ t: "state", v: "paused", seq: 1 },
		{ t: "state", v: "stopped", seq: 1 },
	]);

	expect(dispatched).toEqual([]);
	expect(state.playbackControls.playbackState).toBe("IDLE");
	expect(delivered).toEqual(["NOT_PLAYING", "NOT_PLAYING"]);
});

test("a cast's stop and end are written: the controller's track has no session", () => {
	sdkWrites = false;
	push([
		{ t: "state", v: "stopped", seq: 1, cast: true },
		{ t: "state", v: "completed", seq: 1, cast: true },
	]);

	expect(dispatched).toEqual(["NOT_PLAYING", "IDLE"]);
	expect(delivered).toEqual([]);
});
