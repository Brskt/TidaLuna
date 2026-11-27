// Tests the `playbackControls/PLAY` interceptor in `src/index.ts` on an SDK track. Regression: from
// a stopped player it sent `player.play` before TIDAL's PLAY saga reloaded the track; the reload
// stopped the engine and the next play restarted it, cutting the sound at each Play.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs, initSettled } from "./helpers/boot-stubs";
import type { SentIpc } from "./setup";

const PLAY = "playbackControls/PLAY";

const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({
			playQueue: { currentIndex: 0, elements: [{ mediaItemId: 2, uid: "pq_2" }] },
			playbackControls: { mediaProduct: { productId: "2", referenceId: "pq_2" }, playbackState: "IDLE", prefilled: false },
			content: { mediaItems: { "2": { item: { duration: 215 } } } },
			network: { hasNetworkConnection: true },
		}),
		dispatch: () => {},
		subscribe: () => () => {},
	},
}));

bootStubs();

await import("../src/index");
await initSettled();
const { setSelfLoad } = await import("../src/audio-proxy");

const sent = (globalThis as any).__LUNAR_TEST_IPC__ as SentIpc[];
const playsSent = () => sent.filter((s) => s.channel === "player.play").length;
const native = () => (window as any).NativePlayerComponent;

// The dispatch rail runs the interceptors in order, and the first `true` cancels the action.
const play = () => {
	for (const cb of interceptors[PLAY] ?? []) if (cb(undefined, PLAY) === true) return true;
	return false;
};

let delegate: any;
// The id the delegate gave its latest load, which Rust's refusal names.
const lastLoadId = () => sent.filter((s) => s.channel === "player.load").at(-1)?.args[6];

beforeEach(() => {
	setSelfLoad(false);
	delegate = native().Player();
	// A load of its own leaves no play intent behind from the test before.
	delegate.load("https://example.invalid/start.flac", "flac");
	sent.length = 0;
});

test("a Play TIDAL's reload starts reaches the engine once, from its own play", () => {
	play();
	expect(playsSent(), "sent ahead of the reload").toBe(0);

	// The saga's load, the hard reload `play()` makes of a prefetched track, then the play.
	delegate.load("https://example.invalid/a.flac", "flac");
	delegate.load("https://example.invalid/a.flac", "flac");
	delegate.play();
	expect(playsSent()).toBe(1);
});

test("after Rust refused the load, the listener's Play still resumes the engine", () => {
	delegate.load("https://example.invalid/b.flac", "flac");
	native().refuseLoad(lastLoadId());
	sent.length = 0;

	play();
	expect(playsSent()).toBe(1);
});
