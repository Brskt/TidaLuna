// Tests for the player bridge in `src/index.ts` while a TIDAL Connect controller owns the player.
//
// Regression: the self-load flag names the last load THIS page issued, and a stale one took the
// cast's `completed` for this page's track and advanced the local queue over the cast.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let triggered: { name: string; target: unknown }[] = [];
let announced: unknown[] = [];
// The play queue's current index; 1 makes the SDK's track below the queue's own.
let queueIndex = 0;
// The `context` TIDAL gave the queue's entries: one play-now's, which all its entries share.
const listContext = { id: 9, type: "album" };
let entryContext: object = listContext;

const storeStub = () => ({
	store: {
		getState: () => ({
			playQueue: {
				currentIndex: queueIndex,
				elements: [
					{ mediaItemId: 1, uid: "pq_1", context: entryContext },
					{ mediaItemId: 2, uid: "pq_2", context: entryContext },
				],
			},
			// The track the SDK's loads below are for, its queue entry, and its catalog entry.
			playbackControls: { mediaProduct: { productId: 2, referenceId: "pq_2" } },
			content: { mediaItems: { "2": { item: { duration: 215 } } } },
		}),
		dispatch: (a: Dispatch) => dispatched.push(a),
		// `init` subscribes the committed-device and volume followers; nothing here changes the state.
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

bootStubs();

await import("../src/index");
const { setSelfLoad, proxyReset } = await import("../src/audio-proxy");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
// The advance runs behind a `setTimeout(0)`.
const settle = () => new Promise((r) => setTimeout(r, 0));
const types = () => dispatched.map((d) => d.type);

// The delegate every test replaces, put back after it for the files that run next.
let realPlayback: unknown;

beforeEach(() => {
	dispatched = [];
	triggered = [];
	announced = [];
	queueIndex = 0;
	entryContext = listContext;
	mock.module("../plugins/lib/src/redux/store", storeStub);
	(window as any).NativePlayerComponent = {
		trigger: (name: string, target: unknown) => triggered.push({ name, target }),
		refuseLoad: (id: unknown) => {
			triggered.push({ name: "refuseLoad", target: id });
			return { kind: "pick" };
		},
	};
	realPlayback = (window as any).nativeInterface.playback;
	(window as any).nativeInterface.playback = {
		setCurrentMediaItem: (item: unknown) => announced.push(item),
	};
	// The last load this page issued was a self-load, and nothing a cast does resets it.
	setSelfLoad(true);
});

// The flag is a module global the other files share.
afterEach(() => {
	setSelfLoad(false);
	(window as any).nativeInterface.playback = realPlayback;
});

test("a cast's track ending does not move this page's queue", async () => {
	push([{ t: "state", v: "completed", seq: 5, cast: true }]);
	await settle();

	expect(types()).not.toContain("playQueue/MOVE_NEXT");
	expect(announced).toEqual([]);
});

test("this page's own self-loaded track ending is told to the element TIDAL plays, and moves nothing here", async () => {
	// `ended` on that element is what runs TIDAL's own end-of-track path (the preloaded track's swap,
	// the repeat modes, the autoplay mix after the last track); this page makes no queue move.
	proxyReset();
	const el = document.createElement("audio");
	let ended = 0;
	el.addEventListener("ended", () => ended++);
	el.src = `data:application/dash+xml;base64,${btoa("own-end")}`;
	push([{ t: "state", v: "completed", seq: 5 }]);
	await settle();

	expect(ended).toBe(1);
	expect(types()).not.toContain("playQueue/MOVE_NEXT");
	// TIDAL announces the next item itself, in full, whichever way it moves on.
	expect(announced).toEqual([]);
});

test("a cast's track ending still reaches the player bar", async () => {
	// Only the queue is held back: the bar mirrors the transport whoever drives it.
	push([{ t: "state", v: "completed", seq: 5, cast: true }]);
	await settle();

	expect(dispatched).toContainEqual({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" });
});

test("a load refused for a cast is settled and refused, not left waiting", () => {
	const warn = console.warn;
	const warned: unknown[][] = [];
	console.warn = (...args: unknown[]) => void warned.push(args);
	try {
		push([{ t: "mediacastowned", v: { load: 1 } }]);
	} finally {
		console.warn = warn;
	}

	// 0: the SDK's unknown-duration sentinel.
	expect(triggered).toContainEqual({ name: "mediaduration", target: 0 });
	// `refusePlayback`'s unit: an error code outside TIDAL's map, and STOP, IDLE, the banner in
	// that order.
	expect(triggered).toContainEqual({
		name: "mediaerror",
		target: { error: "a TIDAL Connect controller owns the player", errorCode: "tidalunar_cast_owns_player" },
	});
	expect(types()).toEqual(["playbackControls/STOP", "playbackControls/SET_PLAYBACK_STATE", "message/MESSAGE_ERROR"]);
	expect(dispatched[1]).toEqual({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" });
	// Claimed by its own arm, not warned as an unhandled event.
	expect(warned).toEqual([]);
	// The delegate learns its load was refused before the duration that lets the SDK go on.
	const names = triggered.map((t) => t.name);
	expect(names.indexOf("refuseLoad")).toBeGreaterThanOrEqual(0);
	expect(names.indexOf("refuseLoad")).toBeLessThan(names.indexOf("mediaduration"));
});

// The number the delegate sent with its latest `player.load`, which Rust's answer names.
const loadIdOf = (sent: SentIpc[]) =>
	sent.filter((s) => s.channel === "player.load").at(-1)?.args[6];

// Regression: TIDAL's pick saga (`D6`) loads with autoplay, then plays (`k6 -> n1 ->
// nativePlayer.play()`); the refusal's IDLE sends `k6` through one more load first. That play
// resumed the phone's paused cast under a bar naming the refused pick.
const castRefusal = async () => {
	const { createNativePlayerComponent } = await import("../src/controllers/player");
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	const sdk = component.Player();
	const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
	sdk.load("https://cdn.invalid/media/pick.flac", "flac", "");
	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);
	const channelsFrom = (from: number) => sent.slice(from).map((s) => s.channel);
	return { component, sdk, sent, channelsFrom };
};

test("the SDK's autoplay of a load refused for a cast does not resume the cast", async () => {
	const { sdk, sent, channelsFrom } = await castRefusal();

	const from = sent.length;
	sdk.play();

	expect(channelsFrom(from)).not.toContain("player.play");
});

test("a listener's own Play still reaches the cast after a refused load", async () => {
	// TIDAL's PLAY action (the bar's button, the media keys) reaches `setDesiredPlayback`.
	const { component, sent, channelsFrom } = await castRefusal();

	const from = sent.length;
	component.setDesiredPlayback(true);

	expect(channelsFrom(from)).toContain("player.play");
});

test("the next load after a refusal plays as any load does", async () => {
	// The cast is over by then: Rust takes the load, and the SDK's play that follows it is real.
	const { sdk, sent, channelsFrom } = await castRefusal();

	sdk.load("https://cdn.invalid/media/after.flac", "flac", "");
	const from = sent.length;
	sdk.play();

	expect(channelsFrom(from)).toContain("player.play");
});

// A real delegate under the SDK, the durations it is handed recorded as the SDK's load awaits them.
const castDelegate = async () => {
	const { createNativePlayerComponent } = await import("../src/controllers/player");
	const component = createNativePlayerComponent();
	(window as any).NativePlayerComponent = component;
	const sdk = component.Player();
	const durations: unknown[] = [];
	sdk.addEventListener("mediaduration", (e: { target: unknown }) => durations.push(e.target));
	const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
	const channelsFrom = (from: number) => sent.slice(from).map((s) => s.channel);
	return { component, sdk, sent, durations, channelsFrom };
};
// A listener's play-now of the list the queue holds, as the ADD_TRACK_LIST interceptor arms it.
const pickHere = (component: any) => {
	component.requestPlayOnLoad();
	component.actOnQueue({ act: "pick", context: listContext });
};

test("a refusal answering an earlier load leaves the latest load alone", async () => {
	// Load A is refused, and the renderer sends load B before A's answer lands; the cast ends in
	// between and Rust plays B. Taken for B's, A's answer settled B at 0 under a refusal banner
	// and dropped B's play.
	const { sdk, sent, durations, channelsFrom } = await castDelegate();
	sdk.load("https://cdn.invalid/media/a.flac", "flac", "");
	const loadA = loadIdOf(sent);
	sdk.load("https://cdn.invalid/media/b.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadA } }]);

	expect(durations).toEqual([]);
	expect(types()).not.toContain("message/MESSAGE_ERROR");
	const from = sent.length;
	sdk.play();
	expect(channelsFrom(from)).toContain("player.play");
});

test("a self-load's refusal tells the listener and leaves the SDK's load alone", async () => {
	const { sdk, sent, durations, channelsFrom } = await castDelegate();
	sdk.load("https://cdn.invalid/media/sdk.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: null }]);

	expect(durations).toEqual([]);
	expect(types()).toContain("message/MESSAGE_ERROR");
	const from = sent.length;
	sdk.play();
	expect(channelsFrom(from)).toContain("player.play");
});

test("a pick played from the controller's queue settles the SDK's load as one that ran", async () => {
	// The SDK dispatches its track change once its load hears a length: the bar then names the
	// pick, which is what the cast now plays.
	const { component, sdk, sent, durations, channelsFrom } = await castDelegate();
	pickHere(component);
	sdk.load("https://cdn.invalid/media/pick.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastrouted", v: { load: loadIdOf(sent) } }]);

	expect(durations).toEqual([215]);
	expect(types()).not.toContain("message/MESSAGE_ERROR");
	// The SDK's autoplay is not held back: the pick's play acts on the pick. The pick's load left
	// the play intent on, which would dedup the play below into sending nothing.
	component.setDesiredPlayback(false);
	const from = sent.length;
	sdk.play();
	expect(channelsFrom(from)).toContain("player.play");
});

test("a routed answer to an earlier load leaves the latest load waiting on its own", async () => {
	const { sdk, sent, durations } = await castDelegate();
	sdk.load("https://cdn.invalid/media/a.flac", "flac", "");
	const loadA = loadIdOf(sent);
	sdk.load("https://cdn.invalid/media/b.flac", "flac", "");

	push([{ t: "mediacastrouted", v: { load: loadA } }]);

	expect(durations).toEqual([]);
});

test("only a listener's pick is sent as one", async () => {
	// Rust plays a pick from the controller's queue during a cast. The autoplay mix TIDAL starts
	// at a queue's end plays too, and is no one's pick: during a cast it is refused.
	const { component, sdk, sent } = await castDelegate();
	const flagsOfLatest = () => {
		const args = sent.filter((s) => s.channel === "player.load").at(-1)!.args;
		return { wantPlay: args[4], act: args[7] };
	};

	pickHere(component);
	sdk.load("https://cdn.invalid/media/1.flac", "flac", "");
	expect(flagsOfLatest()).toEqual({ wantPlay: true, act: "pick" });

	component.requestPlayOnLoad();
	sdk.load("https://cdn.invalid/media/2.flac", "flac", "");
	expect(flagsOfLatest()).toEqual({ wantPlay: true, act: "" });

	sdk.load("https://cdn.invalid/media/3.flac", "flac", "");
	expect(flagsOfLatest()).toEqual({ wantPlay: false, act: "" });
});

test("TIDAL's play-now is a pick unless TIDAL started it on its own", async () => {
	const { intentOfTrackList } = await import("../src/controllers/player");

	// A row click, a play button, a playlist or mix played by hand.
	expect(intentOfTrackList({ position: "now" })).toBe("pick");
	expect(intentOfTrackList({ position: "now", context: { isAutoPlay: false } })).toBe("pick");
	// The mix TIDAL's `playbackEnded` saga queues once the play queue runs out.
	expect(intentOfTrackList({ position: "now", context: { isAutoPlay: true } })).toBe("play");
	// Queue edits: no load follows.
	expect(intentOfTrackList({ position: "next" })).toBeNull();
	expect(intentOfTrackList({ position: 3 })).toBeNull();
	// Unknown shapes keep the arm.
	expect(intentOfTrackList(undefined)).toBe("pick");
});

// Regression: when the phone moved on before TIDAL's own load of the cast's track reached Rust,
// the refusal raised TIDAL's banner over music no one here had touched.

test("an automatic load refused for a cast is settled without a banner", async () => {
	const { sdk, sent, durations, channelsFrom } = await castDelegate();
	sdk.load("https://cdn.invalid/media/left.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	// The unknown-duration sentinel, or the SDK's load waits forever.
	expect(durations).toEqual([0]);
	expect(types()).not.toContain("message/MESSAGE_ERROR");
	expect(types()).not.toContain("playbackControls/STOP");
	// Rust ran no load: the SDK's play of it would resume the cast's track.
	const from = sent.length;
	sdk.play();
	expect(channelsFrom(from)).not.toContain("player.play");
});

test("a listener's pick refused for a cast still tells them why", async () => {
	// The counterweight: a track the controller's queue does not hold.
	const { component, sdk, sent, durations } = await castDelegate();
	pickHere(component);
	sdk.load("https://cdn.invalid/media/elsewhere.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	expect(durations).toEqual([0]);
	expect(types()).toContain("message/MESSAGE_ERROR");
});

test("an automatic load refused behind the cast's track reloads the queue's", async () => {
	// The load was for track 2, and this page's queue names 1, the cast's: the phone left 2 before
	// its load arrived, which landed after the one for 1. TIDAL's player holds 2 now.
	const { sdk, sent } = await castDelegate();
	sdk.load("https://cdn.invalid/media/two.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	expect(types()).toContain("playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE");
});

test("an automatic load refused for the queue's own track reloads nothing", async () => {
	queueIndex = 1;
	const { sdk, sent } = await castDelegate();
	sdk.load("https://cdn.invalid/media/two.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	expect(types()).not.toContain("playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE");
});

test("a settled load leaves TIDAL's player in the state the cast's player reported", async () => {
	// Regression: settling left TIDAL's player NOT_PLAYING from its load, and every change of the
	// cast's track showed the cast paused while it played.
	const { sdk, sent } = await castDelegate();
	const events: { name: string; target: unknown }[] = [];
	sdk.addEventListener("mediaduration", (e: { target: unknown }) => events.push({ name: "mediaduration", target: e.target }));
	sdk.addEventListener("mediastate", (e: { target: unknown }) => events.push({ name: "mediastate", target: e.target }));
	push([{ t: "state", v: "active", seq: 9, cast: true }]);
	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");
	events.length = 0;

	push([{ t: "mediacastrouted", v: { load: loadIdOf(sent) } }]);

	expect(events).toEqual([
		{ name: "mediaduration", target: 215 },
		{ name: "mediastate", target: "active" },
	]);
});

// The act the delegate sent with its latest `player.load`.
const actOf = (sent: SentIpc[]) =>
	sent.filter((s) => s.channel === "player.load").at(-1)?.args[7];

test("a pick whose load never came is not spent by a load TIDAL makes on its own", async () => {
	// The pick's saga died before its load; the next load is TIDAL's own, of the cast's track, whose
	// entry the cast's announcement made. Sent as the pick, Rust selected the cast's track again,
	// restarting it.
	const { component, sdk, sent } = await castDelegate();
	pickHere(component);
	entryContext = { type: "UNKNOWN" };

	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");
	expect(actOf(sent)).toBe("");

	// The pick is still the listener's, for a load of its own track.
	entryContext = listContext;
	sdk.load("https://cdn.invalid/media/pick.flac", "flac", "");
	expect(actOf(sent)).toBe("pick");
});

test("a pick counts for one load of its own track", async () => {
	// TIDAL's play step reloads the pick's track once its refusal leaves the player idle: that
	// reload is TIDAL's, and the listener's pick was answered already.
	const { component, sdk, sent } = await castDelegate();
	pickHere(component);
	sdk.load("https://cdn.invalid/media/pick.flac", "flac", "");
	expect(actOf(sent)).toBe("pick");

	sdk.load("https://cdn.invalid/media/pick.flac", "flac", "");
	expect(actOf(sent)).toBe("");
});

test("the play intent is still spent by the next load, whichever it is", async () => {
	const { component, sdk, sent } = await castDelegate();
	const wantPlayOfLatest = () => sent.filter((s) => s.channel === "player.load").at(-1)!.args[4];
	pickHere(component);
	entryContext = { type: "UNKNOWN" };

	sdk.load("https://cdn.invalid/media/other.flac", "flac", "");
	expect(wantPlayOfLatest()).toBe(true);
	sdk.load("https://cdn.invalid/media/other.flac", "flac", "");
	expect(wantPlayOfLatest()).toBe(false);
});

// TIDAL's Next and Previous during a cast: the queue holds the cast's track alone, which TIDAL
// reloads with autoplay. Settled as the track the cast plays, that play resumed a cast the
// phone had paused.
const nextHere = async () => {
	const { interceptors, putInterceptors } = await import("../render/src/exposeTidalInternals.patchAction");
	const run = (registry: Record<string, Set<Function>>) => {
		for (const cb of registry["playQueue/MOVE_NEXT"] ?? []) cb(undefined, "playQueue/MOVE_NEXT");
	};
	return { viaSaga: () => run(putInterceptors), viaDispatch: () => run(interceptors) };
};

test("the player's Next, which TIDAL's saga puts, is sent with the reload it makes", async () => {
	const { sdk, sent } = await castDelegate();
	(await nextHere()).viaSaga();

	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");

	expect(actOf(sent)).toBe("next");
	expect(sent.filter((s) => s.channel === "player.load").at(-1)!.args[4]).toBe(false);
});

test("a Next dispatched on this page is sent with the reload it makes too", async () => {
	const { sdk, sent } = await castDelegate();
	(await nextHere()).viaDispatch();

	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");

	expect(actOf(sent)).toBe("next");
});

test("the reload of a listener's Next is answered by the cast's step: no banner, no resume", async () => {
	const { component, sdk, sent, durations, channelsFrom } = await castDelegate();
	component.actOnQueue({ act: "next", context: listContext });
	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");
	dispatched = [];

	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	// The catalog's length: at 0, TIDAL's play step reloads the track once more.
	expect(durations).toEqual([215]);
	expect(types()).not.toContain("message/MESSAGE_ERROR");
	expect(types()).not.toContain("playbackControls/STOP");
	expect(types()).not.toContain("playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE");
	const from = sent.length;
	sdk.play();
	expect(channelsFrom(from)).not.toContain("player.play");
});

test("a listener's own Play still reaches the cast after its Next", async () => {
	// The counterweight: only the SDK's play of its reload is held back.
	const { component, sdk, sent, channelsFrom } = await castDelegate();
	component.actOnQueue({ act: "next", context: listContext });
	sdk.load("https://cdn.invalid/media/cast.flac", "flac", "");
	push([{ t: "mediacastowned", v: { load: loadIdOf(sent) } }]);

	const from = sent.length;
	component.setDesiredPlayback(true);
	expect(channelsFrom(from)).toContain("player.play");
});

test("a listener's act is bound to the entries it acted on", async () => {
	const { pickOf, moveOf, actOfLoad } = await import("../src/controllers/player");
	const context = { id: 1, type: "playlist" };
	const queue = {
		currentIndex: 0,
		elements: [
			{ uid: "pq_1", context },
			{ uid: "pq_2", context },
			{ uid: "pq_0", context: { type: "UNKNOWN" } },
		],
	};

	expect(pickOf({ position: "now", context })).toEqual({ act: "pick", context });
	expect(pickOf({ position: "now", context: { isAutoPlay: true } })).toBeNull();
	expect(pickOf({ position: "next", context })).toBeNull();
	expect(pickOf({ position: "now" })).toBeNull();
	expect(moveOf("previous", queue)).toEqual({ act: "previous", context });
	expect(moveOf("next", { currentIndex: 0, elements: [] })).toBeNull();
	const pick = { act: "pick" as const, context };
	expect(actOfLoad(pick, queue, "pq_2")).toBe("pick");
	expect(actOfLoad(pick, queue, "pq_0")).toBeNull();
	expect(actOfLoad(pick, queue, "pq_9")).toBeNull();
	expect(actOfLoad(pick, queue, null)).toBeNull();
	expect(actOfLoad(null, queue, "pq_2")).toBeNull();
});
