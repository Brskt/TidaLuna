import { describe, expect, test } from "bun:test";

import {
	acceptSnapshot,
	clickSwitch,
	codecRowNote,
	initialVideoCodecState,
	switchPosition,
	type CodecSnapshot,
	type VideoCodecState,
} from "../plugins/ui/src/videoCodec/state";

const snap = (seq: number, enabled: boolean, phase: CodecSnapshot["phase"]): CodecSnapshot => ({
	seq,
	enabled,
	phase,
});

const heard = (snapshot: CodecSnapshot): VideoCodecState => ({ snapshot, pending: null });

describe("acceptSnapshot", () => {
	test("takes the first snapshot it hears", () => {
		const next = acceptSnapshot(initialVideoCodecState, snap(1, true, { kind: "ready" }));
		expect(next.snapshot).toEqual(snap(1, true, { kind: "ready" }));
	});

	test("keeps the newer of a push and a pull that crossed", () => {
		const state = heard(snap(5, true, { kind: "ready" }));
		expect(acceptSnapshot(state, snap(4, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(5, true, { kind: "downloading" }))).toBe(state);
		expect(acceptSnapshot(state, snap(6, false, { kind: "off" })).snapshot?.seq).toBe(6);
	});

	test("leaves the state alone on a payload it cannot read", () => {
		const state = heard(snap(1, true, { kind: "ready" }));
		for (const payload of [
			null,
			"ready",
			{ seq: 2, enabled: true },
			{ seq: "2", enabled: true, phase: { kind: "ready" } },
			{ seq: 2, enabled: true, phase: { kind: "paused" } },
			{ seq: 2, enabled: true, phase: { kind: "failed", reason: "x" } },
			{ seq: 2, enabled: true, phase: { kind: "failed", reason: "x", recovery: "later" } },
		]) {
			expect(acceptSnapshot(state, payload)).toBe(state);
		}
	});

	test("an echo of the click settles it, a stale one does not", () => {
		const clicked = clickSwitch(heard(snap(1, true, { kind: "ready" })), false);

		const stale = acceptSnapshot(clicked, snap(2, true, { kind: "ready" }));
		expect(stale.pending).toBe(false);

		const echoed = acceptSnapshot(stale, snap(3, false, { kind: "off" }));
		expect(echoed.pending).toBeNull();
	});
});

describe("switchPosition", () => {
	test("shows the click before the echo, then the echo, then the default", () => {
		expect(switchPosition(initialVideoCodecState)).toBe(true);
		const off = heard(snap(1, false, { kind: "off" }));
		expect(switchPosition(off)).toBe(false);
		expect(switchPosition(clickSwitch(off, true))).toBe(true);
	});
});

describe("codecRowNote", () => {
	const note = (phase: CodecSnapshot["phase"]) => codecRowNote(heard(snap(1, true, phase)));

	test("says nothing before the backend answered, nor when the switch already says off", () => {
		expect(codecRowNote(initialVideoCodecState)).toBeNull();
		expect(note({ kind: "off" })).toBeNull();
	});

	test("names each settled phase", () => {
		expect(note({ kind: "starting" })).toBe("Checking the video codec...");
		expect(note({ kind: "downloading" })).toBe("Downloading Cisco's OpenH264 library...");
		expect(note({ kind: "ready" })).toBe("Ready.");
		expect(note({ kind: "unsupported" })).toBe("No video codec is published for this platform.");
	});

	test("tells each recovery its own way, from the backend's phrase", () => {
		expect(note({ kind: "failed", reason: "Cisco's server could not be reached", recovery: "timer" })).toBe(
			"Cisco's server could not be reached. Retrying automatically.",
		);
		expect(
			note({ kind: "failed", reason: "the downloaded video codec failed its integrity check", recovery: "on_demand" }),
		).toBe(
			"The downloaded video codec failed its integrity check. Will try again when needed, or turn the switch off and on to try now.",
		);
		expect(
			note({ kind: "failed", reason: "the video codec could not be loaded on this system", recovery: "never" }),
		).toBe("The video codec could not be loaded on this system. Turn the switch off and on to try again.");
	});
});
