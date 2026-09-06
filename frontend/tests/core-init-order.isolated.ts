// Tests the ORDER inside `initCore()` (frontend/render/src/index.ts), which is the whole
// content of the fix: the Redux store binding is published the moment discovery hands it
// over, not after the config seed.
//
// Driven by `core-init-order.test.ts` in a process of its own, and named without `.test.` to
// keep the suite from collecting it directly: `mock.module` below only bites while nothing has
// imported the real module yet, and two other test files import it transitively.
//
// Why an order deserves its own test: discovery already patches dispatch and exposes the
// state slices, and nothing about the store is unknown by then; only the binding was
// withheld. `seedTidalConfig` fetches and scans every asset the page has loaded and carries
// no timeout, and the SDK's load delegate reads that binding to name the track it is loading.
// A load landing in the gap is sent unnamed, and Rust refuses an unnamed length rather than
// publishing it; the track plays to its end with no duration in the OS media controls.

import { expect, mock, test } from "bun:test";

// `window.core` hangs its getters off this; the app bundle creates it before loading core.
(window as any).luna = (window as any).luna ?? {};

const fakeStore = {
	getState: () => ({}),
	dispatch: () => {},
	subscribe: () => () => {},
};

// What the seed sees when it starts. The seed is the second half of initCore. Reading the
// binding here answers "was it published before the seed finished" with no timing guesswork.
let storeAtSeedTime: unknown = "the seed never ran";
let registryAtSeedTime: string[] = ["the seed never ran"];

mock.module("../render/src/exposeTidalInternals", () => ({
	initTidalInternals: async () => ({ reduxStore: fakeStore }),
	seedTidalConfig: async () => {
		const m = await import("../render/src/modules");
		storeAtSeedTime = m.reduxStore;
		registryAtSeedTime = Object.keys(m.modules);
	},
	tidalModules: {},
	patchDispatch: () => {},
}));

test("the store binding is published before the config seed, and ahead of the registry", async () => {
	const core = await import("../render/src/index");
	await core.initCore();

	expect(storeAtSeedTime).toBe(fakeStore);
	// The negative half, and it is what proves the publication really moved rather than the
	// whole of initModules having been dragged forward with it: the module registry is still
	// empty while the seed runs. Everything `initModules` owns stays behind the seed.
	expect(registryAtSeedTime).toEqual([]);

	// And the registry does get built: the reordering did not drop the second half.
	const m = await import("../render/src/modules");
	expect(Object.keys(m.modules)).toContain("react");
	expect(m.reduxStore).toBe(fakeStore);
});
