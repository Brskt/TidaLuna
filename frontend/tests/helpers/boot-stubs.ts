// Stubs what `src/index.ts` boots from the Luna core and the page: a test imports the bridge
// without dragging in the Luna core. An import added to index.ts that reaches @luna/core
// surfaces as a resolution error in the calling file, which is the intended tripwire.

import { mock } from "bun:test";

export function bootStubs(): void {
	mock.module("../../render/src", () => ({
		initCore: () => {},
		modules: {},
		defineHostModule: () => {},
		LunaPlugin: class {},
	}));
	mock.module("../../plugins/lib/src", () => ({}));
	mock.module("../../src/bootstrap", () => ({}));
	// happy-dom ships no `navigator.mediaSession`, which the state arm writes to.
	mock.module("../../src/controllers/mediasession", () => ({
		setupActionHandlers: () => {},
		updateMetadata: () => {},
		updatePlaybackState: () => {},
	}));
}
