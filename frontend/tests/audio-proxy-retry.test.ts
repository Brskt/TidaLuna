// Tests for `proxyRetry` in `src/audio-proxy.ts`: the listener asked for a refused stream again.
//
// A refusal is held as the stream it refused, since TIDAL assigns the identical manifest after a
// failure and only that one stays refused. A Play on the refused track reloads that same stream;
// the verdict has to be liftable or the reload hangs on a readiness that never comes.

import { afterEach, beforeEach, expect, test } from "bun:test";

import { onProxyBind, proxyFail, proxyReset, proxyRetry, setSelfLoad } from "../src/audio-proxy";
import type { ProxyBind } from "../src/audio-proxy";

const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
const READINESS = ["loadedmetadata", "loadeddata", "canplay", "canplaythrough"];

let binds: ProxyBind[] = [];

beforeEach(() => {
	proxyReset();
	setSelfLoad(true);
	binds = [];
	onProxyBind((kind) => void binds.push(kind));
});

afterEach(() => {
	proxyReset();
	setSelfLoad(false);
	onProxyBind(() => {});
});

const readinessOf = (el: HTMLAudioElement) => {
	const seen: string[] = [];
	for (const name of READINESS) el.addEventListener(name, () => seen.push(name));
	return seen;
};

const refusedThenAssignedAgain = async () => {
	const el = document.createElement("audio");
	el.src = dash("one");
	await flush();
	proxyFail();
	// Shaka unloads the element and the SDK assigns the stream again on a fresh one.
	el.load();
	const next = document.createElement("audio");
	const seen = readinessOf(next);
	binds = [];
	return { next, seen };
};

test("the refused stream assigned again stays refused while the verdict stands", async () => {
	const { next, seen } = await refusedThenAssignedAgain();

	next.src = dash("one");
	await flush();

	expect(binds).toEqual([]);
	expect(seen).toEqual([]);
});

test("the refused stream assigned again once the listener asked for it is a bind, announced ready", async () => {
	const { next, seen } = await refusedThenAssignedAgain();

	proxyRetry();
	next.src = dash("one");
	await flush();

	expect(binds).toEqual(["src"]);
	expect(seen).toEqual(READINESS);
});
