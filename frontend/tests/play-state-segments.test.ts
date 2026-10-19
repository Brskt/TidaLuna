// Tests whose time `PlayState` (`plugins/lib/src/classes/PlayState.ts`) counts for scrobbling.
//
// Two flows reach it and neither waits for the other: Rust's states (`SET_PLAYBACK_STATE`, dispatched
// by the bridge in `src/index.ts`) and the announcement of the next track (`MediaItem.onMediaTransition`,
// fired by `setCurrentMediaItem` after its own requests). A skip therefore shows up in either order,
// and the states name no track: only the `duration` event that opens a load does, which the bridge
// keeps in `__LUNAR_RUST_PRODUCT_ID__`.
//
// The clock is driven by hand; a count is exact unless its assertion states a bound.

import { beforeEach, expect, mock, test } from "bun:test";

type Interceptor = (state: string) => void;
type Item = { id: string; duration: number };
const stateInterceptors: Interceptor[] = [];
const transitionListeners: Array<(item: Item) => void> = [];
let announced: Item | undefined;

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
mock.module("../plugins/lib/src/classes/MediaItem", () => ({
	MediaItem: {
		fromPlaybackContext: async () => announced,
		onMediaTransition: (_unloads: unknown, listener: (item: Item) => void) => void transitionListeners.push(listener),
	},
}));

const { PlayState } = await import("../plugins/lib/src/classes/PlayState");

const SECOND = 1000;
const A: Item = { id: "A", duration: 240 };
const B: Item = { id: "B", duration: 240 };
// Half of 240 s: the threshold both tracks have to reach.
const HALF = 120 * SECOND;

let now = 0;
const epoch = 1_700_000_000_000;
performance.now = () => now;
Date.now = () => epoch + now;
const advance = (ms: number) => void (now += ms);

const scrobbles: Array<{ id: string; counted: number }> = [];

// Rust's `duration` event opens every load, and the bridge keeps the track it names.
const holds = (product: string | undefined) => void ((globalThis as any).__LUNAR_RUST_PRODUCT_ID__ = product);
const state = (next: string) => {
	for (const interceptor of stateInterceptors) interceptor(next);
};
// What `setCurrentMediaItem` produces once its requests have answered.
const announce = (item: Item) => {
	for (const listener of transitionListeners) listener(item);
};

// `onScrobble`'s set-up awaits the item the store names before it listens for transitions.
announced = A;
PlayState.onScrobble(new Set(), (item: Item) => scrobbles.push({ id: item.id, counted: PlayState.cumulativePlaytime }));
for (let i = 0; i < 4; i++) await Promise.resolve();

beforeEach(() => {
	holds(undefined);
	state("IDLE");
	announce(A);
	PlayState.cumulativePlaytime = 0;
	scrobbles.length = 0;
	advance(10 * SECOND);
});

const counted = (id: string) => scrobbles.filter((s) => s.id === id);

test("a skip that is listened to for as long as the track needs is counted on the next track, whichever order", () => {
	for (const pauseFirst of [true, false]) {
		scrobbles.length = 0;
		announce(A);
		PlayState.cumulativePlaytime = 0;
		holds("A");
		state("PLAYING");
		advance(30 * SECOND);
		if (pauseFirst) state("NOT_PLAYING");
		announce(B);
		if (!pauseFirst) state("NOT_PLAYING");
		holds("B");
		state("PLAYING");
		advance(HALF);
		state("IDLE");

		expect(counted("B"), `pause first: ${pauseFirst}`).toHaveLength(1);
		expect(counted("B")[0]!.counted, `pause first: ${pauseFirst}`).toBe(HALF);
	}
});

test("time taken at an announcement is not taken again while the track plays on", () => {
	holds("B");
	state("PLAYING");
	advance(2 * SECOND);
	state("NOT_PLAYING");
	state("PLAYING");
	announce(B);
	expect(PlayState.cumulativePlaytime).toBe(2 * SECOND);

	announce(A);
	announce(B);
	expect(PlayState.cumulativePlaytime).toBe(0);
});

test("time played on a track the listener never went to is not carried to a later visit", () => {
	const C: Item = { id: "C", duration: 240 };
	holds("C");
	state("PLAYING");
	advance(2 * SECOND);
	state("NOT_PLAYING");
	announce(B);

	announce(C);
	expect(PlayState.cumulativePlaytime).toBe(0);
});

test("a track Rust names with something that is not a string is a track it names none", () => {
	holds(5 as unknown as string);
	state("PLAYING");
	advance(SECOND);
	state("NOT_PLAYING");

	expect(PlayState.cumulativePlaytime).toBe(SECOND);
});

test("the time of a track whose pause came after the next announcement does not wait for its replay", () => {
	holds("A");
	state("PLAYING");
	advance(5 * SECOND);
	announce(B);
	advance(100);
	state("NOT_PLAYING");

	announce(A);
	expect(PlayState.cumulativePlaytime).toBe(0);
});

// Every place the announcement can fall among the states.

type Step = { at: number; run: () => void };
const T0 = 1_000_000;

// Plays `steps` (Rust's reports, in the order Rust sent them, at the time each was sent) and the
// announcement of B at `announceAt`, after every report sent at or before it. Returns what A had
// counted when it was scrobbled and what B counted when it ended.
function play(steps: Step[], announceAt: number) {
	holds(undefined);
	state("IDLE");
	now = T0;
	announce(A);
	PlayState.cumulativePlaytime = 0;
	scrobbles.length = 0;
	let announcedYet = false;
	for (const step of steps) {
		if (!announcedYet && announceAt < step.at) {
			now = T0 + announceAt;
			announce(B);
			announcedYet = true;
		}
		now = T0 + step.at;
		step.run();
	}
	if (!announcedYet) {
		now = T0 + announceAt;
		announce(B);
	}
	return { a: counted("A")[0]?.counted, b: PlayState.cumulativePlaytime, scrobbledB: counted("B").length };
}

// A has played the 120 s it needs from 120_000 on (a scrobble of A is expected from there).
const A_LISTENED = 125_000;

// Every moment between two reports, just after one and just before the next, and the moment of
// each, from the time A has been listened to long enough to be scrobbled.
function announcementsAmong(steps: Step[], from: number, to: number) {
	const moments = new Set<number>([A_LISTENED]);
	for (let i = from; i < to; i++) {
		moments.add(steps[i]!.at);
		moments.add(steps[i]!.at + 1);
		moments.add(steps[i + 1]!.at - 1);
	}
	return [...moments].filter((at) => at >= A_LISTENED).sort((x, y) => x - y);
}

test("a skip through the native player is counted per track wherever the announcement falls", () => {
	const steps: Step[] = [
		{ at: 0, run: () => holds("A") },
		{ at: 0, run: () => state("PLAYING") },
		{ at: 170_000, run: () => state("NOT_PLAYING") },
		{ at: 170_400, run: () => holds("B") },
		{ at: 170_900, run: () => state("PLAYING") },
		{ at: 180_900, run: () => state("IDLE") },
	];
	const wrong: string[] = [];
	for (const at of announcementsAmong(steps, 1, 5)) {
		const { a, b, scrobbledB } = play(steps, at);
		// A is the one the listener left: what it played until it stopped, or until the announcement.
		if (a !== Math.min(at, 170_000)) wrong.push(`announced at ${at}: A counted ${a}`);
		if (b !== 10_000 || scrobbledB !== 0) wrong.push(`announced at ${at}: B counted ${b}`);
	}
	expect(wrong).toEqual([]);
});

test("a skip from a self-load, which sends no pause, is counted per track wherever the announcement falls", () => {
	const steps: Step[] = [
		{ at: 0, run: () => holds("A") },
		{ at: 0, run: () => state("PLAYING") },
		{ at: 170_300, run: () => holds("B") },
		{ at: 170_500, run: () => state("PLAYING") },
		{ at: 180_500, run: () => state("IDLE") },
	];
	const wrong: string[] = [];
	for (const at of announcementsAmong(steps, 1, 4)) {
		const { a, b, scrobbledB } = play(steps, at);
		// Nothing says A stopped: its segment goes on to the next track's active, or the announcement.
		if (a !== Math.min(at, 170_500)) wrong.push(`announced at ${at}: A counted ${a}`);
		if (b !== 10_000 || scrobbledB !== 0) wrong.push(`announced at ${at}: B counted ${b}`);
	}
	expect(wrong).toEqual([]);
});

// The same track reported twice, paused and resumed, replayed.

test("an active reported twice for one start loses and doubles nothing", () => {
	holds("A");
	state("PLAYING");
	advance(10 * SECOND);
	advance(48);
	state("PLAYING");
	advance(10 * SECOND);
	state("NOT_PLAYING");

	expect(PlayState.cumulativePlaytime).toBe(20 * SECOND + 48);
});

test("a track replayed after itself is scrobbled and counted afresh", () => {
	holds("A");
	state("PLAYING");
	advance(130 * SECOND);
	announce(A);
	expect(counted("A")).toHaveLength(1);
	advance(20 * SECOND);
	state("NOT_PLAYING");

	expect(PlayState.cumulativePlaytime).toBe(20 * SECOND);
});

test("the first track announced, which played before its announcement, keeps what it played", () => {
	// Nothing announced yet: `currentMediaItem` is what the store named at set-up, and a fresh session names none.
	(PlayState as any).currentMediaItem = undefined;
	PlayState.cumulativePlaytime = 0;
	holds("B");
	state("PLAYING");
	advance(261);
	announce(B);
	advance(SECOND);
	state("NOT_PLAYING");

	expect(PlayState.cumulativePlaytime).toBe(SECOND + 261);
});

// A load Rust named no track for: the segment is the announced track's.

test("a segment of a track Rust named none is credited to the announced track and starts afresh at the announcement", () => {
	state("PLAYING");
	advance(170 * SECOND);
	announce(B);
	advance(100);
	state("NOT_PLAYING");

	expect(counted("A")).toHaveLength(1);
	expect(counted("A")[0]!.counted).toBe(170 * SECOND);
	// Only what ran between the announcement and the pause that closes it: bounded, and never A's 170 s.
	expect(PlayState.cumulativePlaytime).toBeLessThanOrEqual(100);
});

// The public timestamp.

test("lastPlayStart stays the epoch the announced track started playing at, through its announcement", () => {
	holds("B");
	state("PLAYING");
	const startedAt = epoch + now;
	advance(5 * SECOND);
	announce(B);
	advance(5 * SECOND);

	expect(PlayState.lastPlayStart).toBe(startedAt);
});
