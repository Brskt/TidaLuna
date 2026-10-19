// Tests the playtime `PlayState` (`plugins/lib/src/classes/PlayState.ts`) counts for scrobbling.
//
// `lastPlayStart` is public, forwarded by plugins as a timestamp, and stays an epoch. The
// playtime is a duration: counted from the wall clock, a step back of the clock (a VM's clock sync
// does that) would subtract from it, and a step forward would add to it.

import { expect, mock, test } from "bun:test";

type Interceptor = (state: string) => void;
const stateInterceptors: Interceptor[] = [];

mock.module("@luna/core", () => ({ findModuleByProperty: () => undefined }));
mock.module("../plugins/lib/src/index.safe", () => ({
	libTrace: { withSource: () => ({ trace: { err: { withContext: () => ({}) } } }) },
	unloads: new Set(),
}));
mock.module("../plugins/lib/src/redux", () => ({
	RepeatMode: {},
	store: { getState: () => ({}) },
	intercept: (action: string, _unloads: unknown, callback: Interceptor) => {
		if (action === "playbackControls/SET_PLAYBACK_STATE") stateInterceptors.push(callback);
	},
}));
mock.module("../plugins/lib/src/classes/MediaItem", () => ({ MediaItem: {} }));

const { PlayState } = await import("../plugins/lib/src/classes/PlayState");

const setPlaybackState = (state: string) => {
	for (const interceptor of stateInterceptors) interceptor(state);
};

test("a wall clock put back during playback does not shorten the playtime, and lastPlayStart stays an epoch", async () => {
	const realNow = Date.now;
	const epoch = 1_700_000_000_000;
	PlayState.cumulativePlaytime = 0;
	Date.now = () => epoch;
	try {
		setPlaybackState("PLAYING");
		// What plugins read: the wall-clock moment playback started, not a monotonic reading.
		expect(PlayState.lastPlayStart).toBe(epoch);

		Date.now = () => epoch - 3000;
		await new Promise((resolve) => setTimeout(resolve, 60));
		setPlaybackState("NOT_PLAYING");

		// Counted from the wall clock this would be negative: the step back, not the sixty milliseconds.
		expect(PlayState.cumulativePlaytime).toBeGreaterThanOrEqual(50);
		expect(PlayState.cumulativePlaytime).toBeLessThan(1000);
		expect(PlayState.lastPlayStart).toBeUndefined();
	} finally {
		Date.now = realNow;
	}
});
