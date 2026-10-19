// Tests for the `playbackControls/PLAY` and `SEEK` interceptors in `src/index.ts` and the refusal
// record in `src/refuse-playback.ts`.
//
// Regression: after this page refused a track (its own request failed, a 415, an unreadable manifest, a
// load Rust could not make), the listener's Play sent Rust a `player.play` for the self-load flag alone.
// Rust held the track the refusal had stopped, not the refused one, and resumed it under the refused
// one's name for good: TIDAL's own Play had no player left to reload into (the stop reset them all).
// The Play is the page's to answer until Rust holds the refused product again, not until it was
// asked once: a retry whose own request fails leaves Rust with the old track, and TIDAL's PLAY saga
// waits for a transition the refusal left pending.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

const PLAY = "playbackControls/PLAY";

let dispatched: Dispatch[] = [];
let productId = "";
let prefilled = false;
const watchers = new Set<() => void>();
// A store write: what TIDAL's reducers do to `playbackControls` is seen by every subscriber.
const storeChanged = () => [...watchers].forEach((cb) => cb());

const { interceptors } = await import("../render/src/exposeTidalInternals.patchAction");
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: { mediaProduct: { productId }, prefilled } }),
		dispatch: (a: Dispatch) => {
			for (const cb of interceptors[a.type] ?? []) cb((a as any).payload, a.type);
			dispatched.push(a);
		},
		subscribe: (cb: () => void) => (watchers.add(cb), () => void watchers.delete(cb)),
	},
}));

bootStubs();

await import("../src/index");
const { setSelfLoad } = await import("../src/audio-proxy");
const { askRefusedAgain, forgetRefusal, refusePlayback } = await import("../src/refuse-playback");
// `init` registers the interceptors from a `setTimeout(0)`.
await new Promise((r) => setTimeout(r, 50));

const sent = (globalThis as any).__LUNAR_TEST_IPC__ as SentIpc[];
const playsSent = () => sent.filter((s) => s.channel === "player.play").length;
const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);

// The dispatch rail runs the interceptors in order, and the first `true` cancels the action.
const play = () => {
	for (const cb of interceptors[PLAY] ?? []) if (cb(undefined, PLAY) === true) return true;
	return false;
};

let replays = 0;

beforeEach(() => {
	sent.length = 0;
	dispatched = [];
	productId = "";
	prefilled = false;
	replays = 0;
	(window as any).nativeInterface.playback.replaySelfLoad = () => void replays++;
	setSelfLoad(true);
});

afterEach(() => {
	forgetRefusal("");
	watchers.clear();
});

const refuse = (id?: string) => {
	refusePlayback("tidalunar_load_failed", "boom", "could not load", id);
	dispatched = [];
	sent.length = 0;
};

const RELOAD = "playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE";
const lengthOfLoaded = (duration: number) => push([{ t: "duration", v: duration }]);

test("a Play on the product this page refused asks TIDAL to reload it: its transition is what the waiting PLAY saga resumes on", () => {
	// The refused click's SDK load never settles; `prefilled` stays set and the saga waits for a
	// transition nothing else will make.
	productId = "222";
	refuse("222");

	play();

	expect(dispatched).toEqual([{ type: RELOAD }]);
});

test("a Play on the product this page refused is cancelled: TIDAL's PLAY saga would wait for the pending transition and start whatever loads next", () => {
	productId = "222";
	refuse("222");

	expect(play()).toBe(true);
});

test("a Play on a track nothing refused is not cancelled", () => {
	productId = "222";

	expect(play()).toBe(false);
});

test("a Play while the retry has not reached Rust waits: it resumes nothing and asks for nothing twice", () => {
	productId = "222";
	refuse("222");
	play();
	sent.length = 0;
	dispatched = [];

	expect(play()).toBe(true);

	expect(replays).toBe(1);
	expect(playsSent()).toBe(0);
	expect(dispatched).toEqual([]);
});

test("a Play after Rust reported the length of the track it loaded is Rust's again", () => {
	productId = "222";
	refuse("222");
	play();
	lengthOfLoaded(215);
	sent.length = 0;

	play();

	expect(replays).toBe(1);
	expect(playsSent()).toBe(1);
});

test("a zero length is a load that failed: the Play still waits", () => {
	productId = "222";
	refuse("222");
	play();
	lengthOfLoaded(0);
	sent.length = 0;

	play();

	expect(playsSent()).toBe(0);
});

test("a length measured on another track than the refused one is not the retry's: the Play still waits", () => {
	// Rust re-reports the length of the track it kept (a same-track re-assert) while the retry is in flight.
	productId = "222";
	refuse("222");
	play();
	push([{ t: "duration", v: 215, product: "111" }]);
	sent.length = 0;

	play();

	expect(playsSent()).toBe(0);
});

test("a length measured on the refused track is the retry's", () => {
	productId = "222";
	refuse("222");
	play();
	push([{ t: "duration", v: 215, product: "222" }]);
	sent.length = 0;

	play();

	expect(playsSent()).toBe(1);
});

test("a retry that fails again leaves the refusal standing: the next Play asks again, and resumes nothing", () => {
	// The page's own request failing for a reason nothing names (no 415, no unreadable manifest) is
	// refused when the SDK binds the stream, and leaves Rust with the old track.
	productId = "222";
	refuse("222");
	play();
	refuse("222");

	play();

	expect(replays).toBe(2);
	expect(playsSent()).toBe(0);
});

test("a Play on another product than the refused one resumes Rust's track", () => {
	productId = "333";
	refuse("222");

	play();

	expect(replays).toBe(0);
	expect(playsSent()).toBe(1);
});

test("the refused product announced again is the retry's own announcement and keeps the refusal", () => {
	productId = "222";
	refuse("222");

	forgetRefusal("222");
	play();

	expect(replays).toBe(1);
});

test("a load Rust failed for the current track is a refusal the Play asks again", () => {
	// `mediaerror` naming a track this page loaded: the bridge refuses it, and Rust still holds the
	// previous track, which the refusal's stop paused.
	const playback = (window as any).nativeInterface.playback;
	const loadOwner = playback.loadOwner;
	playback.loadOwner = () => "current";
	try {
		productId = "120002099";
		push([{ t: "mediaerror", v: { error: "HTTP 403 Forbidden", errorCode: "no_such_file", productId } }]);
		dispatched = [];
		sent.length = 0;

		play();

		expect(replays).toBe(1);
		expect(playsSent()).toBe(0);
	} finally {
		playback.loadOwner = loadOwner;
	}
});

test("a Play on a track this page did not refuse goes out at once, and its length dispatches nothing", () => {
	productId = "222";

	play();
	lengthOfLoaded(215);

	expect(replays).toBe(0);
	expect(playsSent()).toBe(1);
	expect(dispatched).toEqual([]);
});

const heldPlays = () => dispatched.filter((d) => d.type === PLAY).length;

test("the Play the retry held goes out once Rust holds the track and TIDAL's reload has made its transition", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();

	lengthOfLoaded(215);
	storeChanged();
	expect(heldPlays()).toBe(0);

	prefilled = false;
	storeChanged();
	expect(heldPlays()).toBe(1);
	expect(playsSent()).toBe(1);
});

test("the held Play goes out once", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();
	lengthOfLoaded(215);
	prefilled = false;
	storeChanged();

	storeChanged();
	storeChanged();

	expect(heldPlays()).toBe(1);
});

test("the held Play does not go out before Rust holds the track, whatever TIDAL's transition did", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();

	prefilled = false;
	storeChanged();

	expect(heldPlays()).toBe(0);
	expect(playsSent()).toBe(0);
});

test("the held Play is dropped when the retry fails, whatever asks for the product again afterwards", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();
	refuse("222");
	askRefusedAgain("222");

	lengthOfLoaded(215);
	prefilled = false;
	storeChanged();

	expect(heldPlays()).toBe(0);
});

test("the held Play is dropped when another track is announced", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();

	forgetRefusal("333");
	lengthOfLoaded(215);
	prefilled = false;
	storeChanged();

	expect(heldPlays()).toBe(0);
});

test("the held Play is dropped when the listener has moved to another product", () => {
	productId = "222";
	prefilled = true;
	refuse("222");
	play();
	lengthOfLoaded(215);

	productId = "333";
	prefilled = false;
	storeChanged();

	expect(heldPlays()).toBe(0);
});

const SEEK = "playbackControls/SEEK";
const seeksSent = () => sent.filter((s) => s.channel === "player.seek").length;
const seek = () => {
	for (const cb of interceptors[SEEK] ?? []) cb(4.5, SEEK);
};

test("a seek on the product this page refused is not sent to Rust: its track is the one the refusal left behind", () => {
	productId = "222";
	refuse("222");

	seek();

	expect(seeksSent()).toBe(0);
});

test("a seek on a track nothing refused reaches Rust", () => {
	productId = "222";

	seek();

	expect(seeksSent()).toBe(1);
});

test("a seek on another product than the refused one reaches Rust", () => {
	productId = "333";
	refuse("222");

	seek();

	expect(seeksSent()).toBe(1);
});

test("a seek while the retry has not reached Rust is not sent", () => {
	productId = "222";
	refuse("222");
	play();
	sent.length = 0;

	seek();

	expect(seeksSent()).toBe(0);
});

test("a seek after Rust loaded the retried product reaches Rust", () => {
	productId = "222";
	refuse("222");
	play();
	lengthOfLoaded(215);
	sent.length = 0;

	seek();

	expect(seeksSent()).toBe(1);
});

test("a seek after a refusal that names no product (a cast, a lost network) reaches Rust", () => {
	productId = "222";
	refuse();

	seek();

	expect(seeksSent()).toBe(1);
});

test("the SDK loading the refused product again is the refusal asked for again, once", () => {
	productId = "222";
	refuse("222");

	expect(askRefusedAgain("333")).toBe(false);
	expect(askRefusedAgain("222")).toBe(true);
	expect(askRefusedAgain("222")).toBe(false);
});
