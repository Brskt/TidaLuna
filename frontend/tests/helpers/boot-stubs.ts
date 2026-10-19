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

// Settles once `init` (`src/index.ts`) has registered its interceptors: its last step asks Rust for
// the plugins, which nothing here answers. The import returns before `init` starts (a zero-delay timer).
export const initSettled = (): Promise<void> => (globalThis as any).__LUNAR_TEST_IPC_SENT__("jsrt.load_plugins");
