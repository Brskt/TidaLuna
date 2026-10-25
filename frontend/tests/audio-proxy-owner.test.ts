// Tests for the two-element mirror in `src/audio-proxy.ts`.
//
// Shaka keeps two elements: the one that plays and the one it preloads the next track into. Rust
// speaks for the first only. The second reads as loaded and idle, a write to it reaches nobody, and
// playing it is Shaka's hand-over to the next track, which Rust is not asked about.

import { afterEach, beforeEach, expect, test } from "bun:test";
import type { SentIpc } from "./setup";

import { onHandoff, onSdkLoad, onSdkReload, proxyEnded, proxyFail, proxyIdle, proxyReset, proxySetDuration, proxySetPlaying, proxySetTime, setSelfLoad } from "../src/audio-proxy";

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
const channelsFrom = (from: number) => sent.slice(from).map((s) => s.channel);

let handoffs = 0;

beforeEach(() => {
	proxyReset();
	// Rust plays this page's own self-load, the only track whose reports reach an element.
	setSelfLoad(true);
	handoffs = 0;
	onHandoff(() => void handoffs++);
	onSdkLoad(() => ({ kind: "reload" }));
	onSdkReload(async () => {});
});

afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	onHandoff(() => {});
	onSdkLoad(() => ({ kind: "other" }));
	onSdkReload(async () => {});
});

// The playing element and the preloaded one, past their readiness bursts.
async function pair() {
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = dash("playing");
	preloaded.src = dash("preloaded");
	await flush();
	return { playing, preloaded };
}

test("the first manifest takes the mirror, a second element's is a preload and does not", async () => {
	const { playing, preloaded } = await pair();
	proxySetDuration(200);
	proxySetPlaying(true);
	proxySetTime(42);

	expect([playing.paused, playing.currentTime, playing.duration]).toEqual([false, 42, 200]);
	expect([preloaded.paused, preloaded.currentTime]).toEqual([true, 0]);
});

test("the preloaded element keeps the length Rust reports: Shaka waits 3 s for one it cannot read", async () => {
	const { preloaded } = await pair();
	proxySetDuration(200);

	expect(preloaded.duration).toBe(200);
});

test("a write to the preloaded element reaches nobody", async () => {
	const { preloaded } = await pair();
	const from = sent.length;
	preloaded.currentTime = 0;
	preloaded.pause();

	expect(channelsFrom(from)).toEqual([]);
});

test("playing the preloaded element hands the mirror over without a word to Rust", async () => {
	const { playing, preloaded } = await pair();
	proxySetDuration(200);
	proxySetTime(199.8);
	const from = sent.length;
	await preloaded.play();

	expect(channelsFrom(from)).toEqual([]);
	expect([preloaded.paused, preloaded.currentTime]).toEqual([false, 0]);
	expect(playing.paused).toBe(true);
	expect(handoffs).toBe(1);
	// Rust still reports the previous track: none of it is the newcomer's, up to the next length.
	proxySetTime(200);
	proxySetPlaying(false);
	expect([preloaded.paused, preloaded.currentTime]).toEqual([false, 0]);
	proxySetDuration(180);
	proxySetTime(1);
	expect([preloaded.currentTime, preloaded.duration]).toEqual([1, 180]);
});

test("the end goes to the playing element only, and arms the listener", async () => {
	const { playing, preloaded } = await pair();
	const ended = { playing: 0, preloaded: 0 };
	playing.addEventListener("ended", () => ended.playing++);
	preloaded.addEventListener("ended", () => ended.preloaded++);

	expect(proxyEnded()).toBe(true);
	expect(ended).toEqual({ playing: 1, preloaded: 0 });
	expect(handoffs).toBe(1);
});

test("the listener is armed before `ended` is dispatched: TIDAL answers within it", async () => {
	const { playing } = await pair();
	let armedAtEnd = -1;
	playing.addEventListener("ended", () => (armedAtEnd = handoffs));
	proxyEnded();

	expect(armedAtEnd).toBe(1);
});

test("the first length Rust reports after the hand-over plays the track: the SDK's play came before Rust had it", async () => {
	// A stream that is not DASH is loaded paused and waits for the play an SDK load ends with.
	const { preloaded } = await pair();
	await preloaded.play();
	const from = sent.length;
	proxySetDuration(180);
	proxySetDuration(180);

	expect(channelsFrom(from)).toEqual(["player.play"]);
});

test("a listener who paused after the hand-over is not played by the length", async () => {
	const { preloaded } = await pair();
	await preloaded.play();
	preloaded.pause();
	const from = sent.length;
	proxySetDuration(180);

	expect(channelsFrom(from)).toEqual([]);
});

test("a listener that throws does not break Shaka's play", async () => {
	const { preloaded } = await pair();
	onHandoff(() => {
		throw new Error("listener failed");
	});
	const error = console.error;
	console.error = () => {};
	try {
		await expect(preloaded.play()).resolves.toBeUndefined();
	} finally {
		console.error = error;
	}
	expect(preloaded.paused).toBe(false);
});

test("unloading the playing element releases the mirror and keeps the length", async () => {
	// TIDAL's retry of a prefetched or expired stream on Play reloads the track Rust still plays: no new
	// length is reported for it.
	const { playing } = await pair();
	proxySetDuration(200);
	playing.load();
	playing.src = dash("playing, again");
	await flush();
	proxySetTime(7);

	expect([playing.currentTime, playing.duration]).toEqual([7, 200]);
});

test("unloading the preloaded element leaves the mirror with the playing one", async () => {
	const { playing, preloaded } = await pair();
	proxySetDuration(200);
	proxySetTime(30);
	preloaded.load();

	expect(playing.currentTime).toBe(30);
});

// TIDAL reloads the track it plays without announcing anything (its expired-stream retry on Play, the
// retry once the network is back): Rust is sent nothing and reports no length, and the SDK's load
// waits on one. The `src` of such a reload answers with the length Rust already reported.
const eventsOf = (el: HTMLElement) => {
	const seen: string[] = [];
	for (const type of ["durationchange", "loadedmetadata", "canplaythrough"]) el.addEventListener(type, () => seen.push(type));
	return seen;
};

test("a reload of the track Rust has settled answers with its length, ahead of the readiness burst", async () => {
	const { playing } = await pair();
	proxySetDuration(200);
	playing.load();
	const seen = eventsOf(playing);
	playing.src = dash("playing, signed again");
	await flush();

	expect(seen).toEqual(["durationchange", "loadedmetadata", "canplaythrough"]);
	expect(playing.duration).toBe(200);
});

test("a length of zero settles nothing: Rust has no length to answer with", async () => {
	const { playing } = await pair();
	proxySetDuration(0);
	playing.load();
	const seen = eventsOf(playing);
	playing.src = dash("no length yet");
	await flush();

	expect(seen).toEqual(["loadedmetadata", "canplaythrough"]);
});

test("the track's end, a failure and a reset each unsettle it", async () => {
	for (const unsettle of [() => proxyEnded(), () => proxyFail(), () => proxyReset()]) {
		proxyReset();
		const { playing } = await pair();
		proxySetDuration(200);
		unsettle();
		const seen = eventsOf(playing);
		playing.src = dash("after");
		await flush();
		expect(seen).not.toContain("durationchange");
	}
});

test("a manifest on the other element is Shaka's preload, and is not answered", async () => {
	const { preloaded } = await pair();
	proxySetDuration(200);
	preloaded.load();
	const seen = eventsOf(preloaded);
	preloaded.src = dash("preload again");
	await flush();

	expect(seen).toEqual(["loadedmetadata", "canplaythrough"]);
});

test("the hand-over to the preloaded element unsettles: Rust still has the previous track", async () => {
	const { playing, preloaded } = await pair();
	proxySetDuration(200);
	await preloaded.play();
	playing.load();
	preloaded.load();
	const seen = eventsOf(preloaded);
	preloaded.src = dash("loaded again before Rust has the newcomer");
	await flush();

	expect(seen).toEqual(["loadedmetadata", "canplaythrough"]);
});

test("a probe that throws answers nothing and breaks nothing", async () => {
	onSdkLoad(() => {
		throw new Error("probe failed");
	});
	const { playing } = await pair();
	proxySetDuration(200);
	playing.load();
	const seen = eventsOf(playing);
	const error = console.error;
	console.error = () => {};
	try {
		playing.src = dash("probe throws");
		await flush();
	} finally {
		console.error = error;
	}

	expect(seen).toEqual(["loadedmetadata", "canplaythrough"]);
});

test("an SDK-native track's reports reach no element: a preloaded element is not found past zero", async () => {
	// The native player plays and Shaka preloads the next track into an element meanwhile. Were Rust's
	// position the element's, Shaka's `currentTime = 0` on it (it rewinds a preloaded element it finds
	// past zero) would seek the native track to its start.
	setSelfLoad(false);
	const { playing, preloaded } = await pair();
	let timeupdates = 0;
	preloaded.addEventListener("timeupdate", () => timeupdates++);
	proxySetDuration(200);
	proxySetPlaying(true);
	proxySetTime(190);

	expect([preloaded.paused, preloaded.currentTime, playing.currentTime]).toEqual([true, 0, 0]);
	expect(timeupdates).toBe(0);
});

test("a track that is not self-loaded ends: the mirror reads stopped and keeps its element", async () => {
	const { playing } = await pair();
	proxySetDuration(200);
	proxySetPlaying(true);
	proxySetTime(199);
	proxyIdle();

	expect([playing.paused, playing.currentTime, playing.duration]).toEqual([true, 0, 0]);
	// Shaka may hold the next self-loaded track in it: that track's own end still has an element to say it on.
	let ended = 0;
	playing.addEventListener("ended", () => ended++);
	expect(proxyEnded()).toBe(true);
	expect(ended).toBe(1);
});

test("a track that is not self-loaded ends: Rust no longer has the settled track, and no manifest is answered", async () => {
	const { playing } = await pair();
	proxySetDuration(200);
	proxyIdle();
	playing.load();
	const seen = eventsOf(playing);
	playing.src = dash("after the native track");
	await flush();

	expect(seen).toEqual(["loadedmetadata", "canplaythrough"]);
});
