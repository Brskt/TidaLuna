// Tests that the SDK native player's `stop` and `cancelPreload` delegates (`src/controllers/player.ts`)
// reach Rust only on behalf of a track that player holds.
//
// Regression: TIDAL's announcement of a DASH track makes this page load it into Rust (`player.load_dash`)
// before the SDK's own `load()` gets to `reset()` its players. The native player still holds the session
// of the FLAC it played. Its `reset()` sent `player.stop`, and Rust halted the track the page had just
// loaded. A self-loaded track is stopped by the STOP interceptors (`src/index.ts`), never by that reset.
//
// Driven by `native-stop-ownership.test.ts` in a process of its own, and named without `.test.` to keep
// the suite from collecting it directly: the stand-ins for the store and `getPlaybackInfo` installed
// here are process-wide.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

const STOP = "playbackControls/STOP";

const state: any = {
	playbackControls: { playbackContext: {}, mediaProduct: {} },
	settings: { quality: { streaming: "LOSSLESS" } },
	content: { mediaItems: {}, tracks: {} },
	playQueue: { currentIndex: 0, repeatMode: 0, elements: [] },
};

// The store's `dispatch` as `patchDispatch` wraps it (`render/src/exposeTidalInternals.ts`): the
// interceptors run before the action is recorded.
const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => state,
		dispatch: (a: Dispatch) => {
			for (const cb of interceptors[a.type] ?? []) cb((a as any).payload, a.type);
		},
		subscribe: () => () => {},
	},
}));
mock.module("../render/src/modules", () => ({ storeReady: Promise.resolve() }));
// The page's own request for a DASH track: what makes it load the track into Rust itself.
mock.module("../plugins/lib/src/helpers/getPlaybackInfo", () => ({
	getPlaybackInfo: async (id: string) => ({
		manifestMimeType: "application/dash+xml",
		manifest: { initUrl: `http://cdn/${id}/init.mp4`, segmentUrls: [`http://cdn/${id}/1.m4s`], codec: "mp4a.40.2" },
		audioQuality: "HIGH",
	}),
}));
bootStubs();

await import("../src/index");
const { isSelfLoad, setSelfLoad } = await import("../src/audio-proxy");
// `init` registers the interceptors from a `setTimeout(0)`.
await new Promise((r) => setTimeout(r, 50));

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const count = (channel: string) => sent.filter((s) => s.channel === channel).length;
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const runStop = () => {
	for (const cb of interceptors[STOP] ?? []) cb(undefined, STOP);
};

let player: any;

beforeEach(() => {
	sent.length = 0;
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: "111", referenceId: "pq_111" } };
	player = (window as any).NativePlayerComponent.Player();
});

// The flag is a module global the other files share.
afterEach(() => setSelfLoad(false));

test("the SDK's reset() that follows the announcement's self-load leaves that track playing", async () => {
	// The failing sequence: the FLAC plays, TIDAL announces a DASH track, the page loads it into Rust,
	// then the SDK's load() resets its players. cancelPreload and stop, in the order reset() makes them.
	player.load("https://cdn/111.flac", "flac", "");
	player.preload("https://cdn/222.flac", "flac", "");
	(window as any).nativeInterface.playback.setCurrentMediaItem({ productId: "222", type: "track" });
	await flush();
	expect(count("player.load_dash")).toBe(1);
	expect(isSelfLoad()).toBe(true);

	player.cancelPreload();
	player.stop();
	await flush();

	expect(count("player.stop")).toBe(0);
	// Dropped whoever plays now: the staged next belongs to the native player alone.
	expect(count("player.preload.cancel")).toBe(1);
});

test("a self-load that lands between the SDK's stop and its flush is spared too", async () => {
	player.load("https://cdn/111.flac", "flac", "");
	player.stop();
	setSelfLoad(true);
	await flush();

	expect(count("player.stop")).toBe(0);
});

test("a native load takes Rust back, and the reset() after it stops that track", async () => {
	setSelfLoad(true);
	player.load("https://cdn/333.flac", "flac", "");
	expect(isSelfLoad()).toBe(false);

	player.stop();
	await flush();

	expect(count("player.stop")).toBe(1);
});

test("one STOP of a track the native player holds is one stop of Rust too", async () => {
	player.load("https://cdn/111.flac", "flac", "");
	runStop();
	player.stop();
	await flush();

	expect(count("player.stop")).toBe(1);
});

test("a withheld stop still forgets the loaded track", async () => {
	// The echo rule (a stop and a load of the same stream in one run skip both) must not match the
	// stream of a session the self-load replaced.
	player.load("https://cdn/111.flac", "flac", "");
	setSelfLoad(true);
	player.stop();
	await flush();
	sent.length = 0;

	setSelfLoad(false);
	player.stop();
	player.load("https://cdn/111.flac", "flac", "");
	await flush();

	expect(count("player.load")).toBe(1);
});
