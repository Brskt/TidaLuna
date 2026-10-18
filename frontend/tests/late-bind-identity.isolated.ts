// Tests the classification of the SDK's loads in `src/controllers/playback.ts` (`onSdkLoad`) against
// the proxy: the SDK's load of a track the listener has left binds its stream after the load that
// replaced it, and is answered with that track's catalog length whatever Rust is doing, which may be
// loading the track the listener moved to. Rust's length for that track is no answer to it.
//
// Driven by `late-bind-identity.test.ts` in a process of its own, and named without `.test.` to keep
// the suite from collecting it directly: `getPlaybackInfo` is replaced here for the whole process.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { sdkDashAnswer } from "./helpers/sdk-answer";

const state: any = {
	playbackControls: { playbackContext: {}, mediaProduct: {} },
	settings: { quality: { streaming: "HIGH" } },
	content: { mediaItems: {}, tracks: {} },
};

let asked: string[] = [];
// A product listed here never gets an answer to its own playbackinfo request.
let hold: Record<string, Promise<void>> = {};

mock.module("../render/src/modules", () => ({ storeReady: Promise.resolve() }));
mock.module("../render/src/exposeTidalInternals.patchAction", () => ({ interceptors: {}, buildActions: {} }));
mock.module("../plugins/lib/src/redux/store", () => ({
	store: { getState: () => state, dispatch: () => {}, subscribe: () => () => {} },
}));
mock.module("../plugins/lib/src/helpers/getPlaybackInfo", () => ({
	getPlaybackInfo: async (id: string) => {
		asked.push(id);
		await hold[id];
		return {
			manifestMimeType: "application/vnd.tidal.bts",
			manifest: { urls: [`https://cdn/stream-${id}.mp4`], mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "" },
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

const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;

// The SDK's playbackinfo answer for `id`: afterwards the stream it hands to Shaka names its product.
const sdkAnswers = async (id: string) => {
	await sdkDashAnswer(id);
	return dash(id);
};

const lengthsOf = (el: HTMLAudioElement) => {
	const seen: number[] = [];
	el.addEventListener("durationchange", () => seen.push(el.duration));
	return seen;
};

// The listener's current track in the store, as TIDAL's prefill writes it before the SDK loads it.
const listenerOn = (id: string) => {
	state.playbackControls = { playbackContext: {}, mediaProduct: { productId: id } };
};

// Rust has `id`, reported on the element the SDK bound for it; the page announced it.
async function playing(controller: ReturnType<typeof createPlaybackController>, id: string) {
	listenerOn(id);
	controller.setCurrentMediaItem({ productId: id, type: "track" });
	await flush();
	const el = document.createElement("audio");
	el.src = await sdkAnswers(id);
	await flush();
	proxySetDuration(6, id);
	return el;
}

// The listener moved on to `id`: its announcement and the page's own load, which Rust has not answered yet.
async function movedTo(controller: ReturnType<typeof createPlaybackController>, id: string) {
	listenerOn(id);
	controller.setCurrentMediaItem({ productId: id, type: "track" });
	await flush();
}

beforeEach(() => {
	asked = [];
	hold = {};
	state.content.mediaItems = { "9100": { item: { duration: 7 } }, "9102": { item: { duration: 5 } } };
});
afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	state.playbackControls = { playbackContext: {}, mediaProduct: {} };
});

test("the track Rust still holds, once the listener has left it, is answered with its catalog length", async () => {
	const controller = createPlaybackController();
	const el = await playing(controller, "9100");
	hold["9102"] = new Promise<void>(() => {});
	await movedTo(controller, "9102");
	const lengths = lengthsOf(el);

	el.src = await sdkAnswers("9100");
	await flush();

	// The catalog's length, not the 6 Rust reported for it, and no credential asked for.
	expect(lengths).toEqual([7]);
	expect(asked).toEqual(["9100", "9102"]);
});
