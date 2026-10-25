// Tests the `duration` arm of the bridge in `src/index.ts`: Rust names the track a length was
// measured on, and the element that holds the stream of another track is not answered with it.
//
// The fixtures are the shape `ui/flush.rs` produces: `product` rides on the duration event only.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import { sdkDashAnswer } from "./helpers/sdk-answer";

mock.module("../plugins/lib/src/redux/store", () => ({
	store: { getState: () => ({ playQueue: { elements: [], currentIndex: 0 } }), dispatch: () => {} },
}));
bootStubs();

await import("../src/index");
const { proxyReset, setSelfLoad } = await import("../src/audio-proxy");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;

// An element holding the stream of track `id`, which the SDK's playbackinfo answer named.
async function holding(id: string) {
	await sdkDashAnswer(id);
	const el = document.createElement("audio");
	el.src = dash(id);
	await flush();
	const lengths: number[] = [];
	el.addEventListener("durationchange", () => lengths.push(el.duration));
	return { el, lengths };
}

beforeEach(() => {
	proxyReset();
	setSelfLoad(true);
	(window as any).NativePlayerComponent = { trigger: () => {} };
});
afterEach(() => {
	proxyReset();
	setSelfLoad(false);
});

test("a length measured on the track the element holds is its answer", async () => {
	const { lengths } = await holding("9201");

	push([{ t: "duration", v: 215, seq: 3, product: "9201" }]);

	expect(lengths).toEqual([215]);
});

test("a length measured on another track is not", async () => {
	const { el, lengths } = await holding("9202");

	push([{ t: "duration", v: 215, seq: 3, product: "9203" }]);

	expect(lengths).toEqual([]);
	expect(el.duration).not.toBe(215);
});
