// Tests for the stream identity of Rust's reports in `src/audio-proxy.ts`.
//
// An element holds the stream the SDK bound to it, and the SDK's own playbackinfo answer names that
// stream's product. Rust reports on one track at a time, and its `duration` names the track it
// measured. The SDK's load has no staleness check. It can bind the stream of a track the listener
// has left while Rust already plays the next one: a report of the next track is no answer to that
// load, and none of that track's time, state or end belongs to the element that holds the stale stream.

import { afterEach, beforeEach, expect, test } from "bun:test";
import type { SentIpc } from "./setup";

import { sdkDashAnswer } from "./helpers/sdk-answer";
import { onSdkLoad, proxyEnded, proxyLoading, proxyReset, proxySetDuration, proxySetPlaying, proxySetTime, setSelfLoad } from "../src/audio-proxy";

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};

// The SDK's playbackinfo answer for `id`, as the page's `fetch` shows it to the observer: afterwards
// the stream the SDK hands to Shaka names its product.
const named = async (id: string) => {
	await sdkDashAnswer(id);
	return dash(id);
};

const eventsOf = (el: HTMLAudioElement, names: string[]) => {
	const seen: string[] = [];
	for (const name of names) el.addEventListener(name, () => seen.push(name === "durationchange" ? `${name}:${el.duration}` : name));
	return seen;
};

beforeEach(() => {
	proxyReset();
	setSelfLoad(true);
	onSdkLoad(() => ({ kind: "other" }));
});

afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	onSdkLoad(() => ({ kind: "other" }));
});

// The SDK bound the stream of track `held` while Rust holds track `rust`, reported with `length`.
async function bound(held: string, rust: string, length: number) {
	const el = document.createElement("audio");
	el.src = await named(held);
	await flush();
	proxySetDuration(length, rust);
	return el;
}

test("a length Rust reports for another track is no answer of the element that holds this one's stream", async () => {
	onSdkLoad(() => ({ kind: "late", duration: 4 }));
	const el = document.createElement("audio");
	const seen = eventsOf(el, ["durationchange"]);
	el.src = await named("9001");
	await flush();

	proxySetDuration(5, "9002");

	expect(seen).toEqual(["durationchange:4"]);
	expect(el.duration).toBe(4);
});

test("the time, state and end Rust reports for another track are not the element's", async () => {
	const el = await bound("9003", "9004", 5);
	const seen = eventsOf(el, ["timeupdate", "playing", "pause", "ended"]);

	proxySetTime(7);
	proxySetPlaying(true);
	proxySetPlaying(false);

	expect(seen).toEqual([]);
	expect([el.currentTime, el.paused]).toEqual([0, true]);
	expect(proxyEnded()).toBe(false);
	expect(seen).toEqual([]);
});

test("Rust's track ending is no end of an element that holds another track, and Rust holds nothing after it", async () => {
	const el = await bound("9007", "9008", 5);
	const ended = eventsOf(el, ["ended"]);

	expect(proxyEnded()).toBe(false);
	expect(ended).toEqual([]);

	// Nothing is known of Rust's track now; nothing tells this element from it.
	proxySetTime(2);
	expect(el.currentTime).toBe(2);
});

test("a report that names no track is the element's, as it was", async () => {
	const el = await bound("9009", "9010", 5);
	const seen = eventsOf(el, ["durationchange", "timeupdate"]);

	proxySetDuration(200);
	proxySetTime(3);

	expect(seen).toEqual(["durationchange:200", "timeupdate"]);
});

test("a stream no playbackinfo answer named takes whatever Rust reports", async () => {
	const el = document.createElement("audio");
	el.src = dash("never answered");
	await flush();
	const seen = eventsOf(el, ["durationchange", "timeupdate"]);

	proxySetDuration(5, "9011");
	proxySetTime(3);

	expect(seen).toEqual(["durationchange:5", "timeupdate"]);
});

test("a late bind meets a Rust that is still loading the listener's track, and is answered at once", async () => {
	// The page announced the current track: Rust's is the previous one's until the load reports.
	onSdkLoad(() => ({ kind: "late", duration: 4 }));
	const first = await bound("9012", "9012", 6);
	proxyLoading();
	const seen = eventsOf(first, ["durationchange"]);

	first.src = await named("9013");
	await flush();

	expect(seen).toEqual(["durationchange:4"]);
});

test("a reload of the listener's track waits for Rust's length while Rust is still loading it", async () => {
	onSdkLoad(() => ({ kind: "reload" }));
	const el = await bound("9014", "9014", 6);
	proxyLoading();
	const seen = eventsOf(el, ["durationchange"]);

	el.src = await named("9015");
	await flush();
	expect(seen).toEqual([]);

	proxySetDuration(5, "9015");
	expect(seen).toEqual(["durationchange:5"]);
});

test("a reload is answered with Rust's length, not the catalog length a late bind left on the element", async () => {
	const el = await bound("9016", "9016", 5);
	onSdkLoad(() => ({ kind: "late", duration: 4 }));
	el.src = await named("9017");
	await flush();
	expect(el.duration).toBe(4);
	const seen = eventsOf(el, ["durationchange"]);

	onSdkLoad(() => ({ kind: "reload" }));
	el.src = await named("9016");
	await flush();

	expect(seen).toEqual(["durationchange:5"]);
});

test("the first length after a hand-over that is not the newcomer's does not start it", async () => {
	// Rust re-reports the track it still holds (a same-track re-assert) before the next one's length.
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = await named("9018");
	preloaded.src = await named("9019");
	await flush();
	proxySetDuration(200, "9018");
	await preloaded.play();
	const from = sent.length;

	proxySetDuration(200, "9018");
	proxySetTime(190);
	expect([preloaded.currentTime, sent.slice(from).map((s) => s.channel)]).toEqual([0, []]);

	proxySetDuration(180, "9019");
	proxySetTime(1);

	expect([preloaded.currentTime, preloaded.duration]).toEqual([1, 180]);
	expect(sent.slice(from).map((s) => s.channel)).toEqual(["player.play"]);
});

test("nothing is known of Rust's track once the mirror is reset", async () => {
	for (const forget of [() => proxyReset()]) {
		proxyReset();
		const el = await bound("9020", "9021", 5);
		forget();
		el.src = await named("9022");
		await flush();

		proxySetTime(7);

		expect(el.currentTime).toBe(7);
	}
});
