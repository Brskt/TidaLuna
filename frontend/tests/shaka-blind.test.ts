// Tests for `armOnShakaBlind` in `src/video/shaka-blind.ts`.
//
// Boot order: the bundle is injected after TIDAL's static chunks, so the rewritten vendor chunk may
// set the flag before bootstrap arms, or a chunk imported later sets it after.

import { beforeEach, expect, test } from "bun:test";

const { armOnShakaBlind } = await import("../src/video/shaka-blind");

let installs = 0;
const install = () => void installs++;

beforeEach(() => {
	installs = 0;
	delete (globalThis as any).__lunaShakaBlind;
});

test("nothing installs before the blind flag", () => {
	armOnShakaBlind(install);
	expect(installs).toBe(0);

	// What the rewritten vendor chunk runs last.
	(globalThis as any).__lunaShakaBlind = !0;
	expect(installs).toBe(1);
});

test("a flag the vendor chunk set before bootstrap armed still installs", () => {
	(globalThis as any).__lunaShakaBlind = !0;
	armOnShakaBlind(install);
	expect(installs).toBe(1);
});
