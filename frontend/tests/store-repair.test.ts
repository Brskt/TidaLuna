// Tests `src/store-repair.ts`.
//
// Rust tells the page once what it repaired in settings.db at launch. The banner names what the
// listener will see change: a consent turned off to be safe, native plugins asking again, or
// changes that will not reach the disk this run.

import { expect, test } from "bun:test";

import { storeRepairMessage } from "../src/store-repair";

test("a rebuild that lost settings names both consents and the plugin permissions", () => {
	expect(storeRepairMessage({ turnedOff: ["connect.receiver_always_on", "video.codec_enabled"], trust: "dropped", saved: true })).toBe(
		"TidaLunar repaired its damaged settings. Connect and the video codec were turned off to be safe. Native plugins will ask for permission again.",
	);
});

test("a store kept in memory says changes will not be saved", () => {
	expect(storeRepairMessage({ turnedOff: [], trust: "dropped", saved: false })).toBe(
		"TidaLunar could not repair its settings file. Changes will not be saved until the next launch. Native plugins will ask for permission again.",
	);
});

test("a setting the page has no name for is shown by its key", () => {
	expect(storeRepairMessage({ turnedOff: ["later.consent"], trust: "empty", saved: true })).toBe(
		"TidaLunar repaired its damaged settings. Later.consent was turned off to be safe.",
	);
});

test("a trust table that could not be read says permissions may be asked again", () => {
	expect(storeRepairMessage({ turnedOff: [], trust: "unknown", saved: true })).toBe(
		"TidaLunar repaired its damaged settings. Native plugins may ask for permission again.",
	);
});
