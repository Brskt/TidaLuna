import { describe, expect, test } from "bun:test";

import { acceptSnapshot, clickSwitch, type CodecSnapshot, type VideoCodecState } from "../plugins/ui/src/videoCodec/state";

const snap = (seq: number, enabled: boolean, phase: CodecSnapshot["phase"]): CodecSnapshot => ({
	seq,
	enabled,
	phase,
});

const heard = (snapshot: CodecSnapshot): VideoCodecState => ({ snapshot, pending: null });

describe("acceptSnapshot", () => {
	test("keeps the newer of a push and a pull that crossed", () => {
		const state = heard(snap(5, true, { kind: "ready" }));
		expect(acceptSnapshot(state, snap(4, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(5, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(6, false, { kind: "off" })).snapshot?.seq).toBe(6);
	});

	test("an echo of the click settles it, a stale one does not", () => {
		const clicked = clickSwitch(heard(snap(1, true, { kind: "ready" })), false);

		const stale = acceptSnapshot(clicked, snap(2, true, { kind: "ready" }));
		expect(stale.pending).toBe(false);

		const echoed = acceptSnapshot(stale, snap(3, false, { kind: "off" }));
		expect(echoed.pending).toBeNull();
	});
});
