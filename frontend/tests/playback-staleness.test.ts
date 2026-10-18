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
const listeners = new Set<() => void>();
let subscriptions = 0;
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => state,
		dispatch: (a: Dispatch) => dispatched.push(a),
		subscribe: (cb: () => void) => {
			subscriptions++;
			listeners.add(cb);
			return () => void listeners.delete(cb);
		},
	},
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
const { isSelfLoad, setSelfLoad, proxyEnded, proxyReset, proxySetDuration } = await import("../src/audio-proxy");

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
	state.playbackControls = { playbackContext: {}, mediaProduct: {} };
	listeners.clear();
	subscriptions = 0;
	proxyReset();
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

// A DASH manifest Rust cannot read: `invokeIpc` rejects with the code and the channel it asked.
const manifestFailure = () => Object.assign(new Error("manifest rejected"), { code: 400, channel: "player.parse_dash" });
const stops = () => dispatched.filter((d) => d.type === "playbackControls/STOP");
const banners = () => dispatched.filter((d) => d.type === "message/MESSAGE_ERROR");

test("a DASH manifest Rust cannot read refuses the track that is still current", async () => {
	// The SDK plays a DASH track through the element this page proxies; Rust never hears of
	// it and the SDK's own play resumes what Rust still holds under this track's name.
	const triggered: [string, any][] = [];
	(window as any).NativePlayerComponent = { trigger: (name: string, payload: unknown) => triggered.push([name, payload]) };
	try {
		const controller = createPlaybackController();
		controller.setCurrentMediaItem({ productId: "dash", type: "track" });
		await flush();

		rejectPbi?.(manifestFailure());
		await flush();

		expect(stops()).toHaveLength(1);
		expect(banners()).toHaveLength(1);
		// The code stays outside TIDAL's table, or the refusal skips the track or doubles the banner.
		expect(triggered).toEqual([["mediaerror", expect.objectContaining({ errorCode: "tidalunar_manifest_unreadable" })]]);
		expect((banners()[0].payload as any).message).not.toContain("320 kbps");
	} finally {
		delete (window as any).NativePlayerComponent;
	}
});

test("a failure that does not name the stream refuses nothing, on the fetch channel or off it", async () => {
	// Our request failing says nothing about the SDK's: a FLAC track is loaded by the SDK itself,
	// and stopping it here would stop a healthy track. `tidalFetch` rejects through `invokeIpc` too,
	// with its own channel: only the DASH parse names a stream.
	for (const [productId, failure] of [
		["unknown", new Error("500 Internal Server Error")],
		["blip", Object.assign(new Error("fetch failed"), { code: 500, channel: "tidal.fetch" })],
	] as const) {
		const controller = createPlaybackController();
		controller.setCurrentMediaItem({ productId, type: "track" });
		await flush();

		rejectPbi?.(failure);
		await flush();

		expect(stops()).toEqual([]);
		expect(banners()).toEqual([]);
	}
});

test("a track with no playback info refuses nothing", async () => {
	// 403 and 404 resolve to `undefined`: the SDK's own request gets the same answer and reports it.
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "gone", type: "track" });
	await flush();

	resolvePbi?.(undefined);
	await flush();

	expect(stops()).toEqual([]);
	expect(banners()).toEqual([]);
});

test("a FLAC stream the SDK loads itself is left alone", async () => {
	const from = sent.length;
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "flac", type: "track" });
	await flush();

	resolvePbi?.({
		audioQuality: "LOSSLESS",
		manifestMimeType: "application/vnd.tidal.bts",
		manifest: { urls: ["https://cdn/mediatracks/flac"], mimeType: "audio/flac", codecs: "flac", keyId: "" },
	});
	await flush();

	expect(sentSince(from)).not.toContain("player.load");
	expect(stops()).toEqual([]);
	expect(banners()).toEqual([]);
});

test("a refusal that raises is reported and does not cost the track its context write", async () => {
	const reported: unknown[][] = [];
	const original = console.error;
	console.error = (...args: unknown[]) => void reported.push(args);
	try {
		const controller = createPlaybackController();
		controller.setCurrentMediaItem({ productId: "raises", type: "track" });
		await flush();
		dispatched = { push: () => { throw new Error("reducer failed"); } } as unknown as Dispatch[];

		rejectPbi?.(Object.assign(new Error("undecodable"), { code: 415 }));
		await flush();

		expect(reported.some((args) => args[0] === "[luna:playback] refusal failed:")).toBe(true);
		expect((window as any).__LUNAR_CURRENT_PRODUCT_ID__).toBe("raises");
	} finally {
		console.error = original;
	}
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

// The same-product rule. TIDAL announces a track only when its current item changes; a track that
// starts again as the product Rust already loaded (repeat one, repeat all on a queue of one) comes with
// no announcement. The page armed at the track's end (or Shaka's hand-over) takes the store's
// transition to that product for it and asks for the load.
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const loadsFrom = (from: number) => sentSince(from).filter((c) => c === "player.load").length;

// TIDAL's transition: a new media product and playback context in the store.
const transitionTo = (productId: string) => {
	state.playbackControls = { mediaProduct: { productId }, playbackContext: { actualProductId: productId } } as any;
	for (const l of [...listeners]) l();
};

async function playedAndEnded(controller: ReturnType<typeof createPlaybackController>, id: string) {
	controller.setCurrentMediaItem({ productId: id, type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	proxyReset();
	const el = document.createElement("audio");
	el.src = dash(id);
	proxyEnded();
}

test("a transition to the product that just ended loads it again, once", async () => {
	const from = sent.length;
	const controller = createPlaybackController();
	await playedAndEnded(controller, "again");
	expect(loadsFrom(from)).toBe(1);

	transitionTo("again");
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(2);

	// Disarmed by the replay: a later change of the same product is nobody's replay.
	transitionTo("again");
	await flush();
	expect(loadsFrom(from)).toBe(2);
});

test("a transition to another product is no replay, and disarms the rule", async () => {
	const from = sent.length;
	const controller = createPlaybackController();
	await playedAndEnded(controller, "first");

	transitionTo("second");
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(1);

	transitionTo("first");
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(1);
});

test("the next announcement disarms the rule, and its own context update is no replay", async () => {
	// TIDAL announces the track it moved to, and the announcement writes the playback context once
	// the stream is known: that write is not a transition.
	const from = sent.length;
	const controller = createPlaybackController();
	await playedAndEnded(controller, "first");

	controller.setCurrentMediaItem({ productId: "second", type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(2);

	transitionTo("second");
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(2);
});

test("a hand-over while the next track's announcement is in flight arms nothing", async () => {
	// The listener pressed Next after the preload: TIDAL announced the track, whose load is under way,
	// and Shaka then plays the element it preloaded. The transition that follows is that track's own.
	const from = sent.length;
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "first", type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	controller.setCurrentMediaItem({ productId: "second", type: "track" });
	await flush();
	proxyReset();
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = dash("first");
	preloaded.src = dash("second");
	await preloaded.play();
	expect(subscriptions).toBe(0);

	transitionTo("second");
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	expect(loadsFrom(from)).toBe(2);
});

test("a hand-over arms the rule, and the `completed` that follows it does not arm it again", async () => {
	// Armed again after the transition, the rule would take the announcement's own context update
	// for a replay and load the new track twice.
	const controller = createPlaybackController();
	controller.setCurrentMediaItem({ productId: "first", type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	proxyReset();
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = dash("first");
	preloaded.src = dash("second");
	await preloaded.play();
	expect(subscriptions).toBe(1);

	expect(proxyEnded()).toBe(false);
	expect(subscriptions).toBe(1);
});

// The SDK's own reload of the track it plays (its expired-stream retry on Play, the retry once the
// network is back). The page announces nothing for it and Rust reports no length. The element
// answers the SDK's load itself, for the product the store names and Rust has loaded.
async function settled(controller: ReturnType<typeof createPlaybackController>, id: string, length: number) {
	controller.setCurrentMediaItem({ productId: id, type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	proxyReset();
	const el = document.createElement("audio");
	el.src = dash(id);
	await flush();
	proxySetDuration(length);
	state.playbackControls = { mediaProduct: { productId: id }, playbackContext: {} } as any;
	return el;
}

const reloadSeen = async (el: HTMLAudioElement) => {
	let answered = 0;
	el.addEventListener("durationchange", () => answered++);
	el.load();
	el.src = dash("signed again");
	// A reload of the track Rust has asks for a fresh credential first and is answered once it has one.
	resolvePbi?.(aacStream);
	await flush();
	return answered;
};

test("an announced track is not answered for until Rust reports its length", async () => {
	const controller = createPlaybackController();
	const el = await settled(controller, "playing", 200);
	controller.setCurrentMediaItem({ productId: "next", type: "track" });
	await flush();
	resolvePbi?.(aacStream);
	await flush();
	state.playbackControls = { mediaProduct: { productId: "next" }, playbackContext: {} } as any;

	expect(await reloadSeen(el)).toBe(0);

	proxySetDuration(180);
	expect(await reloadSeen(el)).toBe(1);
});

// A track the SDK plays through Shaka sits on the proxied element, and the SDK's own playbackinfo
// answer names which track: the page refuses a track it did not load into Rust exactly when the
// element holds that track's stream.
const { observeSdkFetch } = await import("../src/sdk-playbackinfo");
const sdkBodies: Record<string, unknown> = {};
// The page's `fetch` as the early runtime's slot shows it to the observer: the request and its answer.
const sdk = {
	fetch: ((url: string) => {
		const answer = Promise.resolve(new Response(JSON.stringify(sdkBodies[String(url)])));
		observeSdkFetch(url, answer);
		return answer;
	}) as typeof fetch,
};

// Returns the stream the SDK then hands to Shaka.
const sdkAnswers = async (id: string, stream = dash(id)) => {
	const url = `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=HIGH`;
	sdkBodies[url] = { trackId: 1, manifestMimeType: "application/dash+xml", manifest: stream.split(",")[1] };
	await sdk.fetch(url);
	await flush();
	return stream;
};
const flacStream = "https://audio.tidal.com/mediatracks/flac";
const sdkAnswersFlac = async (id: string) => {
	const url = `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=LOSSLESS`;
	sdkBodies[url] = {
		trackId: 1,
		manifestMimeType: "application/vnd.tidal.bts",
		manifest: btoa(JSON.stringify({ mimeType: "audio/flac", codecs: "flac", keyId: "", urls: [flacStream] })),
	};
	await sdk.fetch(url);
	await flush();
};
const bindStream = async (el: HTMLAudioElement, stream: string) => {
	el.src = stream;
	await flush();
};
const readinessOfEl = (el: HTMLAudioElement) => {
	const seen: string[] = [];
	for (const name of ["loadedmetadata", "canplay"]) el.addEventListener(name, () => seen.push(name));
	return seen;
};
const announce = async (controller: ReturnType<typeof createPlaybackController>, id: string, type = "track", options?: any) => {
	controller.setCurrentMediaItem({ productId: id, type }, options);
	await flush();
};
const ours = (what: "blip" | "forbidden" | "bts" | "incomplete-dash") => {
	if (what === "blip") rejectPbi?.(new Error("fetch failed"));
	else if (what === "bts") rejectPbi?.(new SyntaxError("Unexpected token"));
	else if (what === "forbidden") resolvePbi?.(undefined);
	else resolvePbi?.({ audioQuality: "HIGH", manifestMimeType: "application/dash+xml", manifest: { initUrl: "", segmentUrls: [], codec: "mp4a.40.2" } });
};
const refusedTracks = () => ({ stops: dispatched.filter((d) => d.type === "playbackControls/STOP").length, banners: dispatched.filter((d) => d.type === "message/MESSAGE_ERROR").length });

test("our request failing while the SDK takes the track to the element refuses it, and its readiness is withheld", async () => {
	const stream = await sdkAnswers("2017");
	const controller = createPlaybackController();
	await announce(controller, "2017");
	ours("blip");
	await flush();
	// The element holds nothing yet: the SDK may still fail, or play it natively.
	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });

	const el = document.createElement("audio");
	const seen = readinessOfEl(el);
	await bindStream(el, stream);

	expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
	expect(seen).toEqual([]);
});

test("every way this page ends without loading the track refuses it once the element holds it", async () => {
	for (const [what, id] of [["forbidden", "2009"], ["bts", "2010"], ["incomplete-dash", "2011"]] as const) {
		dispatched = [];
		const stream = await sdkAnswers(id);
		const controller = createPlaybackController();
		await announce(controller, id);
		ours(what);
		await flush();
		await bindStream(document.createElement("audio"), stream);

		expect({ what, ...refusedTracks() }).toEqual({ what, stops: 1, banners: 1 });
		proxyReset();
	}
});

test("the stream bound before the SDK's answer was read refuses the track when the answer lands", async () => {
	const controller = createPlaybackController();
	await announce(controller, "2020");
	ours("blip");
	await flush();
	const stream = dash("2020");
	await bindStream(document.createElement("audio"), stream);
	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });

	await sdkAnswers("2020");

	expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
});

test("a late bind of a superseded track is not the next track's, whatever happens to the next", async () => {
	// The SDK's load has no staleness check: A's stream can bind after B was announced.
	const streamA = await sdkAnswers("2005");
	const streamB = await sdkAnswers("2006");
	const controller = createPlaybackController();
	await announce(controller, "2005");
	ours("blip");
	await announce(controller, "2006");
	const el = document.createElement("audio");

	await bindStream(el, streamA);
	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });

	// B's own request fails alone while the element still holds A: not B's stream.
	ours("blip");
	await flush();
	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });

	await bindStream(el, streamB);
	expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
});

// The late bind meets a settled Rust track and a store that still names it (the SDK's transition to
// the superseded track comes only after its load): the stream names the product it is for.
const lateBind = async (stream: string, id: string) => {
	const controller = createPlaybackController();
	const el = await settled(controller, id, 200);
	const lengths: number[] = [];
	el.addEventListener("durationchange", () => lengths.push(el.duration));
	pbiCalls = [];
	el.load();
	el.src = stream;
	// A reload asks for a fresh credential first, and is answered once it has one.
	resolvePbi?.(aacStream);
	await flush();
	return lengths;
};

test("the SDK's late load of a superseded track settles on that track's catalog length, and Rust's credential is not asked for", async () => {
	(state.content as any).mediaItems = { "3001": { item: { duration: 190 } } };
	try {
		const lengths = await lateBind(await sdkAnswers("3001"), "3002");

		expect(lengths).toEqual([190]);
		expect(pbiCalls).toEqual([]);
	} finally {
		state.content.mediaItems = {};
	}
});

test("a late load of a track the catalog has no length for settles on 0, the SDK's unknown length", async () => {
	const lengths = await lateBind(await sdkAnswers("3003"), "3004");

	expect(lengths).toEqual([0]);
	expect(pbiCalls).toEqual([]);
});

test("a stream of the product Rust has is its reload, whatever the SDK's answer said", async () => {
	const lengths = await lateBind(await sdkAnswers("3005"), "3005");

	expect(lengths).toEqual([200]);
	expect(pbiCalls).toEqual(["3005"]);
});

test("a stream this page never saw answered is taken for a reload of Rust's track", async () => {
	const lengths = await lateBind(dash("never answered"), "3006");

	expect(lengths).toEqual([200]);
	expect(pbiCalls).toEqual(["3006"]);
});

test("a native track whose request failed alone does not refuse the shaka track that follows", async () => {
	// The next track's hand-over binds before its announcement, and the native track's wait is gone.
	await sdkAnswersFlac("2007");
	const controller = createPlaybackController();
	await announce(controller, "2007");
	ours("blip");
	await flush();

	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	await bindStream(playing, dash("X8"));
	await bindStream(preloaded, await sdkAnswers("2008"));
	await preloaded.play();
	await announce(controller, "2008");
	resolvePbi?.(aacStream);
	await flush();

	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });
});

test("a gapless hand-over that came before the announcement is refused when its request fails", async () => {
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	await bindStream(playing, dash("X9"));
	await bindStream(preloaded, await sdkAnswers("2022"));
	await preloaded.play();
	const controller = createPlaybackController();
	await announce(controller, "2022");
	ours("blip");
	await flush();

	expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
});

test("a stream this page never saw answered names no track, and refuses nothing", async () => {
	const controller = createPlaybackController();
	await announce(controller, "2012");
	ours("blip");
	await flush();
	await bindStream(document.createElement("audio"), dash("never answered"));

	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });
});

test("a video, and a track the receiver already plays, are never refused for the element they sit on", async () => {
	const stream = await sdkAnswers("2000");
	const controller = createPlaybackController();
	const el = document.createElement("audio");
	await bindStream(el, stream);
	await announce(controller, "2000", "video");
	await announce(controller, "2001", "track", { alreadyPlaying: true });
	await bindStream(el, await sdkAnswers("2001"));

	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });
});

test("one refusal, however many of the failure, the bind and the answer say the same", async () => {
	const stream = await sdkAnswers("2014");
	const controller = createPlaybackController();
	await announce(controller, "2014");
	const el = document.createElement("audio");
	await bindStream(el, stream);
	ours("blip");
	await flush();
	await bindStream(el, `${stream}`);
	await sdkAnswers("2014");

	expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
});

// The refusals this page decides itself, besides the element's rule: each leaves the SDK free to
// bind a stream for the same announcement afterwards. The element still holds the product's
// previous stream, and the refusal comes before the SDK's new bind: that bind is the same
// announcement's and must not announce itself ready.
const undecodable = () => Object.assign(new Error("undecodable"), { code: 415 });
const refusals = [
	["an undecodable profile", "2040", undecodable],
	["our request failing", "2042", () => new Error("fetch failed")],
] as const;

for (const [what, id, failure] of refusals) {
	test(`${what} withholds the replay stream the SDK binds after the refusal`, async () => {
		const first = await sdkAnswers(id);
		const controller = createPlaybackController();
		await announce(controller, id);
		resolvePbi?.(aacStream);
		await flush();
		const el = document.createElement("audio");
		await bindStream(el, first);
		dispatched = [];

		controller.replaySelfLoad();
		await flush();
		rejectPbi?.(failure());
		await flush();
		expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });

		const seen = readinessOfEl(el);
		await bindStream(el, await sdkAnswers(id, dash(`${id} signed again`)));

		expect(seen).toEqual([]);
		expect(refusedTracks()).toEqual({ stops: 1, banners: 1 });
	});
}

test("the late failure of an earlier announcement of a product is not its later announcement's", async () => {
	// `lastTransitionId` is a scalar: A, B, A satisfies its guard for the first A's rejection.
	const stream = await sdkAnswers("2024");
	const controller = createPlaybackController();
	await announce(controller, "2024");
	const rejectFirst = rejectPbi;
	await announce(controller, "2025");
	await announce(controller, "2024");
	resolvePbi?.(aacStream);
	await flush();
	await bindStream(document.createElement("audio"), stream);

	rejectFirst?.(new Error("fetch failed"));
	await flush();

	expect(refusedTracks()).toEqual({ stops: 0, banners: 0 });
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
