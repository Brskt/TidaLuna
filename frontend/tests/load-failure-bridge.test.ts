// Tests for the player bridge in `src/index.ts` when Rust reports a load that failed before its
// track played (`mediaerror` carrying the product it names).
//
// A track this page loaded itself is unknown to the SDK. The page answers its failure: raw, the
// event reaches no listener while TIDAL's native player is unbuilt. A failure of a track the SDK
// loaded stays TIDAL's, with the payload it always had.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let triggered: { name: string; target: unknown }[] = [];
let asked: unknown[] = [];
let verdict: "current" | "superseded" | "foreign" = "foreign";

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => dispatched.push(a),
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);
bootStubs();

await import("../src/index");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const types = () => dispatched.map((d) => d.type);

// The delegate every test replaces, put back after it for the files that run next.
let realPlayback: unknown;

beforeEach(() => {
	dispatched = [];
	triggered = [];
	asked = [];
	verdict = "foreign";
	mock.module("../plugins/lib/src/redux/store", storeStub);
	(window as any).NativePlayerComponent = {
		trigger: (name: string, target: unknown) => triggered.push({ name, target }),
	};
	realPlayback = (window as any).nativeInterface.playback;
	(window as any).nativeInterface.playback = {
		setCurrentMediaItem: () => {},
		loadOwner: (productId: unknown) => {
			asked.push(productId);
			return verdict;
		},
	};
});

afterEach(() => {
	(window as any).nativeInterface.playback = realPlayback;
});

const failure = { t: "mediaerror", v: { error: "HTTP 403 Forbidden", errorCode: "no_such_file", productId: "120002099" } };

test("a load this page made that failed refuses its track, under a code TIDAL has no entry for", () => {
	verdict = "current";
	push([failure]);

	expect(asked).toEqual(["120002099"]);
	// One `mediaerror`, and it is the refusal's: the raw one would map to NPO01 beside it.
	expect(triggered).toEqual([
		{ name: "mediaerror", target: { error: "HTTP 403 Forbidden", errorCode: "tidalunar_load_failed" } },
	]);
	expect(types()).toEqual(["playbackControls/STOP", "playbackControls/SET_PLAYBACK_STATE", "message/MESSAGE_ERROR"]);
});

test("a load this page made that failed after the listener moved on refuses nothing and says nothing", () => {
	// The listener's track is another one, possibly the SDK's, and a refusal would stop it.
	verdict = "superseded";
	push([failure]);

	expect(triggered).toEqual([]);
	expect(dispatched).toEqual([]);
});

test("a load the SDK made that failed reaches TIDAL's native player as it always did, named or not", () => {
	verdict = "foreign";
	push([failure, { t: "mediaerror", v: { error: "decode failed", errorCode: "unreadable_file" } }]);

	// `productId` is the bridge's own: the SDK's player reads `error` and `errorCode`.
	expect(asked).toHaveLength(2);
	expect(asked[1]).toBeUndefined();
	expect(triggered).toEqual([
		{ name: "mediaerror", target: { error: "HTTP 403 Forbidden", errorCode: "no_such_file" } },
		{ name: "mediaerror", target: { error: "decode failed", errorCode: "unreadable_file" } },
	]);
	expect(dispatched).toEqual([]);
});
