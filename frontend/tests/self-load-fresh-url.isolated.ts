// Tests the self-load `setCurrentMediaItem` (`src/controllers/playback.ts`) sends for a track it plays
// itself, as far as the stream url it hands Rust: a track replayed hours after it first played must
// be loaded from a url TIDAL signed for the replay, not the one that answered the first load.
//
// Driven by `self-load-fresh-url.test.ts` in a process of its own, and named without `.test.` to
// keep the suite from collecting it directly: `playback-staleness.test.ts` replaces
// `getPlaybackInfo` for the whole process, and this test needs the real one beside the real
// `TidalApi`. TIDAL's answer, the store and the modules around them are stood in for.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

let requests: string[] = [];

// A playbackinfo answer whose stream url names the request that produced it.
const btsAnswer = (nth: number) => ({
	status: 200,
	statusText: "OK",
	json: async () => ({
		manifestMimeType: "application/vnd.tidal.bts",
		manifest: btoa(JSON.stringify({ mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "", urls: [`https://cdn/stream-${nth}.mp4`] })),
	}),
});

const realIpc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...realIpc,
	tidalFetch: async (url: string) => {
		requests.push(url);
		return btsAnswer(requests.length);
	},
}));
mock.module("../plugins/lib/src/index.safe", () => ({
	libTrace: { withSource: () => ({ trace: { err: { withContext: () => ({ throw: (m: string) => Promise.reject(new Error(m)) }) } } }) },
}));

const state = {
	session: { clientId: "cid", countryCode: "NZ" },
	playbackControls: { playbackContext: {}, mediaProduct: {} },
	settings: { language: "en_US", quality: { streaming: "LOW" } },
};
const store = { getState: () => state, dispatch: () => {}, subscribe: () => () => {} };
mock.module("../plugins/lib/src/redux/store", () => ({ store }));
mock.module("../plugins/lib/src/redux", () => ({ store }));
mock.module("../render/src/modules", () => ({ storeReady: Promise.resolve() }));
mock.module("../render/src/exposeTidalInternals.patchAction", () => ({ interceptors: {}, buildActions: {} }));
mock.module("../src/controllers/mediasession", () => ({
	setupActionHandlers: () => {},
	updateMetadata: () => {},
	updatePlaybackState: () => {},
}));

const { createPlaybackController } = await import("../src/controllers/playback");
const { setSelfLoad, proxyReset } = await import("../src/audio-proxy");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
// The controller schedules itself with setTimeout(0), then waits on `TidalApi.rateGate`, which spaces
// requests 1/12 s apart: a few such turns let a transition run to its `player.load`.
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 120));
};
const loadedUrls = () => sent.filter((s) => s.channel === "player.load").map((s) => s.args[0]);

beforeEach(() => {
	requests = [];
	sent.length = 0;
});
afterEach(() => {
	proxyReset();
	setSelfLoad(false);
});

test("a replayed track is loaded from a url signed for the replay", async () => {
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "7001", type: "track" });
	await flush();

	controller.replaySelfLoad();
	await flush();

	expect(loadedUrls()).toEqual(["https://cdn/stream-1.mp4", "https://cdn/stream-2.mp4"]);
	expect(requests).toHaveLength(2);
});
