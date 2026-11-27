// TIDAL's loudness normalization (`normalize-volume.ts`, `adjustedVolume`):
// each track's factors travel to Rust with the track, and the "Normalize volume" mode follows the
// store live. A wrong factor or a missed toggle is silent: the track just plays at another loudness.

import { expect, test } from "bun:test";

import { factorsOf, followNormalization, normalizeVolume } from "../src/loudness";

test("a factor is the gain plus 4 dB of pre-amp, never above 1", () => {
	expect(normalizeVolume(-4)).toBe(1);
	expect(normalizeVolume(-10)).toBeCloseTo(0.501187, 6);
	expect(normalizeVolume(2)).toBe(1);
});

test("a gain the answer leaves out or at zero plays the track untouched, as TIDAL's truthy check does", () => {
	expect(factorsOf({ album: -10, track: undefined })).toEqual([normalizeVolume(-10), 1]);
	expect(factorsOf({ album: 0, track: -8 })).toEqual([1, normalizeVolume(-8)]);
	expect(factorsOf(undefined)).toEqual([1, 1]);
});

test("the mode reaches Rust at start and on every change, and only then", () => {
	const sent: unknown[] = [];
	let state: any = { settings: { audioNormalization: "ALBUM" } };
	const subscribers: (() => void)[] = [];
	const store = {
		getState: () => state,
		subscribe: (cb: () => void) => {
			subscribers.push(cb);
			return () => {};
		},
	};
	followNormalization(store, (mode) => sent.push(mode));
	expect(sent).toEqual(["ALBUM"]);

	state = { settings: { audioNormalization: "ALBUM" }, other: 1 };
	subscribers.forEach((cb) => cb());
	expect(sent).toEqual(["ALBUM"]);

	state = { settings: { audioNormalization: "NONE" } };
	subscribers.forEach((cb) => cb());
	expect(sent).toEqual(["ALBUM", "NONE"]);
});
