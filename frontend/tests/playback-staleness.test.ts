// Tests for `setCurrentMediaItem` in `src/controllers/playback.ts`: staleness, the product kind,
// and a track the Connect receiver already plays.
//
// Staleness: the closure re-reads `lastTransitionId` after every suspension point, a rejection's
// included; without that, track A's late failure acts on track B.
//
// Kind: the MEDIA_PRODUCT_TRANSITION payload a plugin sees is the one this closure builds, not
// TIDAL's, and a video's fields are right only if it writes them.
//
// All three live here because `mock.module` is process-global in bun: a second file mocking
// `render/src/modules` would strip `defineHostModule` from whatever runs after it.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let pbiCalls: string[] = [];
let rejectPbi: ((reason: unknown) => void) | null = null;
let resolvePbi: ((value: unknown) => void) | null = null;

const state = {
	playbackControls: { playbackContext: {}, mediaProduct: {} },
	settings: { quality: { streaming: "LOW" } },
	content: { mediaItems: {}, tracks: {} },
};

mock.module("../render/src/modules", () => ({ storeReady: Promise.resolve() }));
mock.module("../render/src/exposeTidalInternals.patchAction", () => ({
	interceptors: {},
	buildActions: {},
}));
mock.module("../plugins/lib/src/redux/store", () => ({
	store: { getState: () => state, dispatch: (a: Dispatch) => dispatched.push(a) },
}));
mock.module("../plugins/lib/src/helpers/getPlaybackInfo", () => ({
	getPlaybackInfo: (id: string) => {
		pbiCalls.push(id);
		return new Promise((resolve, reject) => {
			resolvePbi = resolve;
			rejectPbi = reject;
		});
	},
}));
mock.module("../src/controllers/mediasession", () => ({
	setupActionHandlers: () => {},
	updateMetadata: () => {},
	updatePlaybackState: () => {},
}));

const { createPlaybackController } = await import("../src/controllers/playback");
const { isSelfLoad, setSelfLoad } = await import("../src/audio-proxy");

// The closure schedules itself with setTimeout(0) and awaits inside; two macrotask turns let
// it reach its first suspension point and, later, run its resumption to completion.
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};

beforeEach(() => {
	dispatched = [];
	pbiCalls = [];
	rejectPbi = null;
	resolvePbi = null;
	(window as any).__LUNAR_CURRENT_PRODUCT_ID__ = undefined;
});

// Reset even when an assertion failed: the seeded context and the self-load flag (a module global
// the other files share) must not reach the next test.
afterEach(() => {
	state.playbackControls.playbackContext = {};
	setSelfLoad(false);
});

const contextOf = (calls: Dispatch[]) => calls.find((d) => d.type === "playbackControls/UPDATE_PLAYBACK_CONTEXT")?.payload as any;

test("a late refusal from a superseded track leaves the current one alone", async () => {
	// Track A is undecodable, but its playbackinfo only rejects after the listener has already
	// picked track B. Everything the catch does (stopping the transport, banner, proxy latch)
	// and everything the tail does after it (the current-product id, UPDATE_PLAYBACK_CONTEXT,
	// the transition interceptors) would land on B, which is playable and playing.
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "A", type: "track" });
	await flush();
	expect(pbiCalls).toEqual(["A"]);

	controller.setCurrentMediaItem({ productId: "B", type: "track" });
	dispatched = [];

	rejectPbi?.(Object.assign(new Error("undecodable"), { code: 415 }));
	await flush();

	expect(dispatched.filter((d) => d.type === "playbackControls/STOP")).toEqual([]);
	expect(dispatched.filter((d) => d.type === "message/MESSAGE_ERROR")).toEqual([]);
	expect((window as any).__LUNAR_CURRENT_PRODUCT_ID__).not.toBe("A");
});

test("a refusal for the track that is still current is acted on", async () => {
	// The counterweight. The guard must key on staleness alone: a genuine 415 for the track a
	// listener is looking at still has to stop the transport and say why.
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "solo", type: "track" });
	await flush();

	rejectPbi?.(Object.assign(new Error("undecodable"), { code: 415 }));
	await flush();

	expect(dispatched.filter((d) => d.type === "playbackControls/STOP")).toHaveLength(1);
	expect(dispatched.filter((d) => d.type === "message/MESSAGE_ERROR")).toHaveLength(1);
});

test("a video is never asked for playback info", async () => {
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "vid", type: "video" });
	await flush();

	expect(pbiCalls).toEqual([]);
});

test("a video says it is one in the context it publishes", async () => {
	// A null here made every consumer treat the video as a track, `fetchBestQuality` included.
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "vid", type: "video" });
	await flush();

	expect(contextOf(dispatched)).toBeDefined();
	expect(contextOf(dispatched)?.actualVideoQuality).not.toBeNull();
});

test("a track after a video asks for its playback info and claims no video", async () => {
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "vid", type: "video" });
	await flush();

	dispatched = [];
	controller.setCurrentMediaItem({ productId: "aud", type: "track" });
	await flush();
	resolvePbi?.({ audioQuality: "LOSSLESS" });
	await flush();

	expect(pbiCalls).toEqual(["aud"]);
	expect(contextOf(dispatched)?.actualVideoQuality).toBeNull();
});

test("a video after a track claims none of the track's audio fields", async () => {
	// A 24/192 track followed by a video had the video announcing the track's audio fields.
	// The previous context is SEEDED: no dispatch writes back to this store, so a track played
	// first would leave `playbackContext` empty and the test passing whatever the code inherits.
	state.playbackControls.playbackContext = {
		actualAudioQuality: "HI_RES_LOSSLESS",
		bitDepth: 24,
		sampleRate: 192000,
	};

	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "vid", type: "video" });
	await flush();

	const ctx = contextOf(dispatched);
	expect(ctx?.actualAudioQuality).toBeNull();
	expect(ctx?.bitDepth).toBeNull();
	expect(ctx?.sampleRate).toBeNull();
});

// A non-FLAC BTS answer at HIGH: the case this delegate self-loads.
const aacStream = {
	audioQuality: "HIGH",
	manifestMimeType: "application/vnd.tidal.bts",
	manifest: { urls: ["https://cdn/mediatracks/cast.mp4?token=SECRET"], mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "" },
};
const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const sentSince = (from: number) => sent.slice(from).map((s) => s.channel);

test("a track the Connect receiver already plays is announced, never loaded again", async () => {
	// Loaded again here, the receiver's track goes out in this page's name: Rust refuses it under
	// the cast with the banner for a load no one here made, and `isSelfLoad` is armed.
	setSelfLoad(false);
	const from = sent.length;
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "cast", type: "track" }, { alreadyPlaying: true });
	await flush();
	resolvePbi?.(aacStream);
	await flush();

	expect(sentSince(from)).toContain("player.metadata");
	expect(sentSince(from)).not.toContain("player.load");
	expect(sentSince(from)).not.toContain("player.load_dash");
	expect(isSelfLoad()).toBe(false);
});

test("a DASH track the Connect receiver already plays is not loaded again either", async () => {
	// The other self-load road. Under the cast Rust refuses `player.load_dash` as it does
	// `player.load`, and the refusal raises the banner over the receiver's track.
	setSelfLoad(false);
	const from = sent.length;
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "cast-dash", type: "track" }, { alreadyPlaying: true });
	await flush();
	resolvePbi?.({
		audioQuality: "HIGH",
		manifestMimeType: "application/dash+xml",
		manifest: { initUrl: "https://cdn/init.mp4", segmentUrls: ["https://cdn/1.mp4"], codec: "mp4a.40.2" },
	});
	await flush();

	expect(sentSince(from)).not.toContain("player.load_dash");
	expect(isSelfLoad()).toBe(false);
});

test("a track TIDAL moves to is still self-loaded", async () => {
	const from = sent.length;
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "local", type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();

	expect(sentSince(from)).toContain("player.load");
	expect(isSelfLoad()).toBe(true);
});
