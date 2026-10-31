import { describe, expect, test } from "bun:test";

import {
	acceptSnapshot,
	clickSwitch,
	initialAudioCacheState,
	pickSize,
	settleClick,
	sizePosition,
	switchPosition,
	type AudioCacheState,
	type CacheSnapshot,
} from "../plugins/ui/src/audioCache/state";

const GIB = 1024 ** 3;

const snap = (seq: number, enabled: boolean, gb: number, usedMb = 0): CacheSnapshot => ({
	seq,
	enabled,
	maxBytes: gb * GIB,
	usedText: `${usedMb} MB`,
	available: true,
	lastFailure: null,
});

const heard = (snapshot: CacheSnapshot): AudioCacheState => ({ ...initialAudioCacheState, snapshot });

describe("settleClick", () => {
	test("the reply settles each click even when the gate refuses it, a pushed snapshot never does", () => {
		const clicked = pickSize(clickSwitch(heard(snap(1, true, 2)), false), 4);

		// A push carrying the clicked values proves neither: it can predate the clicks.
		const pushed = acceptSnapshot(clicked, snap(3, false, 4, 10));
		expect(pushed.pendingEnabled).toBe(false);
		expect(pushed.pendingGb).toBe(4);

		const switched = settleClick(pushed, "enabled", pushed.enabledClick, { reply: snap(4, false, 4, 10) });
		expect(switched.pendingEnabled).toBeNull();
		expect(switched.pendingGb).toBe(4);

		const resized = settleClick(switched, "gb", switched.gbClick, { reply: snap(5, false, 4, 10) });
		expect(resized.pendingGb).toBeNull();
		expect(sizePosition(resized)).toBe(4);

		const reclicked = acceptSnapshot(clickSwitch(resized, true), snap(7, true, 4, 10));
		const overtaken = settleClick(reclicked, "enabled", reclicked.enabledClick, { reply: snap(6, true, 4, 10) });
		expect(overtaken.snapshot?.seq).toBe(7);
		expect(overtaken.pendingEnabled).toBeNull();
	});

	test("an earlier click's reply or refusal leaves a later one held, the latest's refusal lets go", () => {
		let state = clickSwitch(heard(snap(1, false, 2)), true);
		const first = state.enabledClick;
		state = clickSwitch(state, false);
		state = settleClick(state, "enabled", first, { reply: snap(2, true, 2) });
		expect(state.snapshot?.seq).toBe(2);
		expect(switchPosition(state)).toBe(false);
		state = settleClick(state, "enabled", first, "rejected");
		expect(state.pendingEnabled).toBe(false);
		state = settleClick(state, "enabled", state.enabledClick, "rejected");
		expect(state.pendingEnabled).toBeNull();
		expect(switchPosition(state)).toBe(true);
	});
});
