import { describe, expect, test } from "bun:test";

import {
	acceptSnapshot,
	clickSwitch,
	initialVideoCodecState,
	settleClick,
	switchPosition,
	type CodecSnapshot,
	type VideoCodecState,
} from "../plugins/ui/src/videoCodec/state";

const snap = (seq: number, enabled: boolean, phase: CodecSnapshot["phase"]): CodecSnapshot => ({
	seq,
	enabled,
	phase,
});

const heard = (snapshot: CodecSnapshot): VideoCodecState => ({ ...initialVideoCodecState, snapshot });

describe("acceptSnapshot", () => {
	test("keeps the newer of a push and a pull that crossed", () => {
		const state = heard(snap(5, true, { kind: "ready" }));
		expect(acceptSnapshot(state, snap(4, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(5, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(6, false, { kind: "off" })).snapshot?.seq).toBe(6);
	});
});

describe("settleClick", () => {
	test("a click is held until the latest one ends, whatever earlier replies and pushes carry", () => {
		let state = clickSwitch(heard(snap(1, true, { kind: "ready" })), false);
		const first = state.click;
		state = clickSwitch(state, true);
		state = settleClick(state, first, { reply: snap(2, false, { kind: "off" }) });
		expect(state.pending).toBe(true);
		expect(state.snapshot?.seq).toBe(2);
		state = acceptSnapshot(state, snap(3, true, { kind: "ready" }));
		state = acceptSnapshot(state, snap(4, false, { kind: "off" }));
		expect(switchPosition(state)).toBe(true);
		state = settleClick(state, state.click, "rejected");
		expect(state.pending).toBeNull();
		expect(switchPosition(state)).toBe(false);
	});
});
