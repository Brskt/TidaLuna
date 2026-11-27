// Tests what the bridge in `src/index.ts` keeps of the `duration` event for `PlayState`: the track
// Rust holds. A state event names no track, and `PlayState` needs to know whose time a segment is when
// `active` opens it; the `duration` that opens every load is the only report that says.
//
// The fixtures are the shape `ui/flush.rs` produces.

import { expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

mock.module("../plugins/lib/src/redux/store", () => ({
	store: { getState: () => ({ playQueue: { elements: [], currentIndex: 0 } }), dispatch: () => {} },
}));
bootStubs();

await import("../src/index");

const push = (events: unknown[]) => {
	(window as any).NativePlayerComponent = { trigger: () => {} };
	(window as any).__TIDALUNAR_PLAYER_PUSH__(events);
};
const held = () => (window as any).__LUNAR_RUST_PRODUCT_ID__;

test("a load with no length still names its track", () => {
	push([{ t: "duration", v: 215, seq: 3, product: "9302" }]);
	push([{ t: "duration", v: 0, seq: 4, product: "9303" }]);

	expect(held()).toBe("9303");
});

test("states, positions and the end of a track leave it as it was", () => {
	push([{ t: "duration", v: 215, seq: 3, product: "9307" }]);
	push([
		{ t: "state", v: "active", seq: 3 },
		{ t: "time", v: 12, seq: 3 },
		{ t: "state", v: "paused", seq: 4 },
		{ t: "state", v: "completed", seq: 5 },
	]);

	expect(held()).toBe("9307");
});

test("a state that belongs to a cast leaves no track named: Connect's ids are not this page's", () => {
	push([{ t: "duration", v: 215, seq: 3, product: "9309" }]);
	push([{ t: "state", v: "active", seq: 3, cast: true }]);

	expect(held()).toBeUndefined();
});

test("in a batch, the state behind a duration reads the track that duration named", async () => {
	const read: unknown[] = [];
	const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");
	const key = "playbackControls/SET_PLAYBACK_STATE";
	const previous = interceptors[key];
	// What `PlayState` reads the track through when `active` opens its segment.
	interceptors[key] = new Set([(payload: unknown) => void read.push([payload, held()])]);
	try {
		push([{ t: "duration", v: 215, seq: 3, product: "9308" }, { t: "state", v: "active", seq: 3 }]);
	} finally {
		interceptors[key] = previous;
	}

	expect(read).toEqual([["PLAYING", "9308"]]);
});
