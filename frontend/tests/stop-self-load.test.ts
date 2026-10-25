// Tests for the `playbackControls/STOP` interceptors in `src/index.ts`.
//
// Regression: a STOP reached the SDK's `reset()` and nothing else. The native player's `reset()` stops
// Rust only for a session it loaded itself. A track this page loaded (`player.load`, `player.load_dash`)
// kept playing under a stopped transport: after a refusal, and after a queue clear.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs, initSettled } from "./helpers/boot-stubs";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

const STOP = "playbackControls/STOP";

// The store's `dispatch` as `patchDispatch` wraps it (`render/src/exposeTidalInternals.ts`): the
// interceptors run on every dispatched action.
const { interceptors, putInterceptors } = await import("../render/src/exposeTidalInternals.patchAction");
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => {
			for (const cb of interceptors[a.type] ?? []) cb((a as any).payload, a.type);
		},
		subscribe: () => () => {},
	},
}));

bootStubs();

await import("../src/index");
const { setSelfLoad } = await import("../src/audio-proxy");
await initSettled();

const sent = (globalThis as any).__LUNAR_TEST_IPC__ as SentIpc[];
const stopsSent = () => sent.filter((s) => s.channel === "player.stop").length;
const runRegistry = (registry: Record<string, Set<Function>>) => {
	for (const cb of registry[STOP] ?? []) cb(undefined, STOP);
};

beforeEach(() => {
	sent.length = 0;
});

test("a STOP stops the track this page loaded, on the dispatch rail and on the saga's put rail", () => {
	for (const [rail, registry] of [
		["a dispatched STOP", interceptors],
		["a STOP TIDAL's saga puts, after a queue clear", putInterceptors],
	] as const) {
		sent.length = 0;
		setSelfLoad(true);
		runRegistry(registry);

		expect(stopsSent(), rail).toBe(1);
	}
});
