// Tests for the notifications of `src/audio-proxy.ts`: the SDK commits a track to the proxied
// element in two ways, a manifest assigned to the owner ("src") and Shaka playing the element it
// preloaded ("handover"), and the page is told of each with the stream the element now holds.

import { afterEach, beforeEach, expect, test } from "bun:test";

import { onProxyBind, proxyOwnerSrc, proxyReset, setSelfLoad } from "../src/audio-proxy";
import type { ProxyBind } from "../src/audio-proxy";

const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

let binds: [ProxyBind, string | null][] = [];

beforeEach(() => {
	proxyReset();
	setSelfLoad(true);
	binds = [];
	onProxyBind((kind) => void binds.push([kind, proxyOwnerSrc()]));
});

afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	onProxyBind(() => {});
});

test("a manifest on a second element while the first holds the mirror is a preload, not a bind", async () => {
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = dash("playing");
	preloaded.src = dash("preloaded");
	await flush();

	expect(binds).toEqual([["src", dash("playing")]]);
	expect(proxyOwnerSrc()).toBe(dash("playing"));
});

test("playing the preloaded element is a hand-over, and the owner's stream is then the newcomer's", async () => {
	const playing = document.createElement("audio");
	const preloaded = document.createElement("audio");
	playing.src = dash("playing");
	preloaded.src = dash("preloaded");
	await flush();
	binds = [];

	await preloaded.play();

	expect(binds).toEqual([["handover", dash("preloaded")]]);
});

test("a listener that throws does not reach the setter or the hand-over", async () => {
	const errors: unknown[][] = [];
	const original = console.error;
	console.error = (...args: unknown[]) => void errors.push(args);
	try {
		onProxyBind(() => {
			throw new Error("listener failed");
		});
		const playing = document.createElement("audio");
		const preloaded = document.createElement("audio");
		playing.src = dash("playing");
		preloaded.src = dash("preloaded");
		await preloaded.play();

		expect(errors.map((e) => e[0])).toEqual(["[audio-proxy] bind listener failed:", "[audio-proxy] bind listener failed:"]);
	} finally {
		console.error = original;
	}
});
