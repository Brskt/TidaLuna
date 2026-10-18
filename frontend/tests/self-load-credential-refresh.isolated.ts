// Tests the credential refresh of `src/controllers/playback.ts`: the SDK reloads the track it plays
// when its own stream expired (a Play long after the pause) or was loaded paused, answers itself from
// Rust's track and sends Rust nothing. Rust keeps fetching a BTS track with the signed url it was
// handed. The page owes it a fresh one, before the SDK's play.
//
// Driven by `self-load-credential-refresh.test.ts` in a process of its own, and named without
// `.test.` to keep the suite from collecting it directly: `getPlaybackInfo` is replaced here for the
// whole process, as `playback-staleness.test.ts` does with its own stand-in.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

const state: any = {
	playbackControls: { playbackContext: {}, mediaProduct: {} },
	settings: { quality: { streaming: "HIGH" } },
	content: { mediaItems: {}, tracks: {} },
};

// What `getPlaybackInfo` is asked, and how it answers: a stream url that names the ask.
let asked: string[] = [];
let kind: Record<string, "bts" | "dash" | "fails"> = {};
let hold: Promise<void> | null = null;

mock.module("../render/src/modules", () => ({ storeReady: Promise.resolve() }));
mock.module("../render/src/exposeTidalInternals.patchAction", () => ({ interceptors: {}, buildActions: {} }));
mock.module("../plugins/lib/src/redux/store", () => ({
	store: { getState: () => state, dispatch: () => {}, subscribe: () => () => {} },
}));
mock.module("../plugins/lib/src/helpers/getPlaybackInfo", () => ({
	getPlaybackInfo: async (id: string) => {
		asked.push(id);
		const nth = asked.length;
		await hold;
		const mode = kind[id] ?? "bts";
		if (mode === "fails") throw new Error("fetch failed");
		if (mode === "dash") {
			return { manifestMimeType: "application/dash+xml", manifest: { initUrl: `https://cdn/init-${nth}.mp4`, segmentUrls: ["https://cdn/seg.mp4"], codec: "mp4a.40.2" } };
		}
		return {
			manifestMimeType: "application/vnd.tidal.bts",
			manifest: { urls: [`https://cdn/stream-${nth}.mp4`], mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "" },
		};
	},
}));
mock.module("../src/controllers/mediasession", () => ({
	setupActionHandlers: () => {},
	updateMetadata: () => {},
	updatePlaybackState: () => {},
}));

const { createPlaybackController } = await import("../src/controllers/playback");
const { proxyReset, proxySetDuration, setSelfLoad } = await import("../src/audio-proxy");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const loads = () => sent.filter((s) => s.channel === "player.load");

// A track this page loaded, bound by the SDK to an element and reported by Rust; the element is
// returned, for the SDK to bind a stream to it again.
async function loaded(controller: ReturnType<typeof createPlaybackController>) {
	controller.setCurrentMediaItem({ productId: "P", type: "track" });
	const element = document.createElement("audio");
	element.src = dash("first");
	await flush();
	proxySetDuration(200);
	return element;
}

// The SDK binding a stream for `product` once Rust has the track: `loaded`, then this.
async function reloadedFor(product: string, controller: ReturnType<typeof createPlaybackController>) {
	const element = await loaded(controller);
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: product } };
	const order: string[] = [];
	element.addEventListener("durationchange", () => order.push(`answered after ${loads().length} loads`));
	element.src = dash("reload");
	await flush();
	return order;
}

beforeEach(() => {
	asked = [];
	kind = {};
	hold = null;
	sent.length = 0;
});
afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	state.playbackControls = { playbackContext: {}, mediaProduct: {} };
});

test("a reload of the track this page loaded hands Rust a fresh url before the SDK's load is answered", async () => {
	const order = await reloadedFor("P", createPlaybackController());

	expect(asked).toEqual(["P", "P"]);
	expect(loads().map((s) => s.args[0])).toEqual(["https://cdn/stream-1.mp4", "https://cdn/stream-2.mp4"]);
	// Same track again, keeping the play instance: no restart, no play intent, product named.
	expect(loads()[1]!.args.slice(3, 6)).toEqual([false, false, "P"]);
	expect(order).toEqual(["answered after 2 loads"]);
});

test("a DASH track is fetched whole by Rust and is not asked for again", async () => {
	kind["P"] = "dash";
	const order = await reloadedFor("P", createPlaybackController());

	expect(asked).toEqual(["P"]);
	expect(sent.filter((s) => s.channel === "player.load_dash")).toHaveLength(1);
	expect(loads()).toHaveLength(0);
	expect(order).toEqual(["answered after 0 loads"]);
});

test("the SDK loading another product than the one Rust has asks for nothing", async () => {
	await reloadedFor("Q", createPlaybackController());

	expect(asked).toEqual(["P"]);
	expect(loads()).toHaveLength(1);
});

test("a refresh the listener moved on from while it was asked for is dropped", async () => {
	const controller = createPlaybackController();
	const element = await loaded(controller);
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: "P" } };

	const release = Promise.withResolvers<void>();
	hold = release.promise;
	element.src = dash("reload");
	await flush();
	expect(asked).toEqual(["P", "P"]);

	controller.setCurrentMediaItem({ productId: "N", type: "track" });
	release.resolve();
	await flush();

	expect(loads().map((s) => s.args[0])).not.toContain("https://cdn/stream-2.mp4");
});

test("a refresh that fails leaves Rust its track and still answers the SDK", async () => {
	const controller = createPlaybackController();
	const element = await loaded(controller);
	kind["P"] = "fails";
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: "P" } };
	let answered = false;
	element.addEventListener("durationchange", () => (answered = true));
	element.src = dash("reload");
	await flush();

	expect(loads()).toHaveLength(1);
	expect(answered).toBe(true);
});

test("a request that never answers does not hold the SDK's load for ever", async () => {
	const controller = createPlaybackController();
	const element = await loaded(controller);
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: "P" } };
	hold = new Promise<void>(() => {});
	const answeredAt = new Promise<number>((resolve) => element.addEventListener("durationchange", () => resolve(performance.now())));
	const from = performance.now();
	element.src = dash("reload");

	// Answered, but not before the wait the credential was given.
	expect((await answeredAt) - from).toBeGreaterThanOrEqual(2900);
});
