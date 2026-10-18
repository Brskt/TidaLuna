// Tests for what Rust reports while no element holds its track yet, in `src/audio-proxy.ts`.
//
// The SDK binds a track's stream when its own playbackinfo request answers, which can be later than
// the load this page sent Rust (its request slower than ours, or Shaka between its unload and its
// next `src`). Rust's length then reports to no element, and the bind that follows waits on a length
// with no timeout: what Rust measured has to be there when it comes.

import { afterEach, beforeEach, expect, test } from "bun:test";

import { sdkDashAnswer } from "./helpers/sdk-answer";
import { onSdkLoad, onSdkReload, proxyEnded, proxyReset, proxySetDuration, proxySetPlaying, proxySetTime, setSelfLoad } from "../src/audio-proxy";

const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};

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
	onSdkLoad(() => ({ kind: "reload" }));
	onSdkReload(async () => {});
});

afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	onSdkLoad(() => ({ kind: "other" }));
	onSdkReload(async () => {});
});

test("a length Rust reports before the SDK binds the track is the answer of the bind that follows", async () => {
	proxySetDuration(6, "9100");

	const el = document.createElement("audio");
	const seen = eventsOf(el, ["durationchange"]);
	el.src = await named("9100");
	await flush();

	expect(seen).toEqual(["durationchange:6"]);
});

test("the length of another track is not the answer of that bind", async () => {
	proxySetDuration(6, "9101");

	const el = document.createElement("audio");
	const seen = eventsOf(el, ["durationchange"]);
	el.src = await named("9102");
	await flush();

	expect(seen).toEqual([]);
});

test("a track Rust finished while no element held it is not left as the answer of a later bind", async () => {
	proxySetDuration(6, "9105");
	expect(proxyEnded()).toBe(false);

	const el = document.createElement("audio");
	const seen = eventsOf(el, ["durationchange"]);
	el.src = await named("9105");
	await flush();

	expect(seen).toEqual([]);
});

test("a length reported while the page's last load is not its own is nobody's answer", async () => {
	setSelfLoad(false);
	proxySetDuration(6, "9106");
	setSelfLoad(true);

	const el = document.createElement("audio");
	const seen = eventsOf(el, ["durationchange"]);
	el.src = await named("9106");
	await flush();

	expect(seen).toEqual([]);
});

test("a track of unknown length that Rust names is the element's: its time, state and end are delivered", async () => {
	// The shared decoder found no frame count: the length is 0, and the report still names the track,
	// which is what tells the element holding that track's stream that Rust's reports are its own.
	proxySetDuration(5, "9107");
	const el = document.createElement("audio");
	el.src = await named("9108");
	await flush();
	const seen = eventsOf(el, ["timeupdate", "playing", "ended"]);

	proxySetTime(1);
	expect(seen).toEqual([]);

	proxySetDuration(0, "9108");
	proxySetTime(2);
	proxySetPlaying(true);
	expect(proxyEnded()).toBe(true);

	expect(seen).toEqual(["timeupdate", "playing", "ended"]);
});
