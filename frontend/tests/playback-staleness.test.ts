// Tests for the staleness discipline of `setCurrentMediaItem` in `src/controllers/playback.ts`.
//
// The closure runs under a captured `productId` and re-reads the shared `lastTransitionId`
// after every suspension point. A rejection resumes the same way a resolution does, so the
// catch has to re-check too: without it, track A's late failure acts on track B.

import { beforeEach, expect, mock, test } from "bun:test";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let pbiCalls: string[] = [];
let rejectPbi: ((reason: unknown) => void) | null = null;

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
		return new Promise((_, reject) => {
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

// The closure schedules itself with setTimeout(0) and awaits inside; two macrotask turns let
// it reach its first suspension point and, later, run its resumption to completion.
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};

beforeEach(() => {
	dispatched = [];
	pbiCalls = [];
	rejectPbi = null;
	(window as any).__LUNAR_CURRENT_PRODUCT_ID__ = undefined;
});

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
