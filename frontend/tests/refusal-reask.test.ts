// Tests for the `playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION` interceptor in `src/index.ts`.
//
// TIDAL puts that action at the start of every load it begins, naming the product. A product this
// page refused that TIDAL starts loading again is the listener asking for it again. The SDK's own
// retries of a request it could not get answered put nothing, and are not an ask.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

const PREFILL = "playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION";

let current = "";

const { interceptors, putInterceptors } = await import("../render/src/exposeTidalInternals.patchAction");
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: { mediaProduct: { productId: current } } }),
		dispatch: (a: Dispatch) => {
			for (const cb of interceptors[a.type] ?? []) cb((a as any).payload, a.type);
		},
		subscribe: () => () => {},
	},
}));
bootStubs();

await import("../src/index");
const { forgetRefusal, refusePlayback } = await import("../src/refuse-playback");
// `init` registers the interceptors from a `setTimeout(0)`.
await new Promise((r) => setTimeout(r, 50));

let replays = 0;

const prefill = (productId: unknown) => {
	for (const cb of putInterceptors[PREFILL] ?? []) cb({ mediaProduct: { productId }, playbackContext: { actualDuration: 0 } }, PREFILL);
};

beforeEach(() => {
	replays = 0;
	current = "";
	(window as any).nativeInterface = { playback: { replaySelfLoad: () => void replays++ } };
});

afterEach(() => forgetRefusal(""));

const refused = (productId: string) => {
	current = productId;
	refusePlayback("tidalunar_load_failed", "boom", "could not load", productId);
};

test("the refused product loaded again by TIDAL is asked for again, once", () => {
	refused("9301");

	prefill("9301");
	prefill("9301");

	expect(replays).toBe(1);
});

test("nothing refused, nothing asked for; nor for a product another refusal does not name", () => {
	prefill("9304");
	refused("9302");
	prefill("9303");

	expect(replays).toBe(0);
});

test("a prefill that names no product asks for nothing", () => {
	refused("9305");

	for (const payload of [undefined, {}, { mediaProduct: {} }, { mediaProduct: { productId: undefined } }]) {
		for (const cb of putInterceptors[PREFILL] ?? []) cb(payload, PREFILL);
	}

	expect(replays).toBe(0);
});

test("a product id TIDAL holds as a number is the refusal's product", () => {
	refused("9306");

	prefill(9306);

	expect(replays).toBe(1);
});
