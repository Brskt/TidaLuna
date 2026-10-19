// Preloaded by `bunfig.toml` before any test module is imported.
//
// Two module-load-time couplings force this to run first: `src/audio-proxy.ts` reads
// `HTMLMediaElement.prototype`'s `src` descriptor as it loads, and `src/ipc.ts` captures
// `window.cefQuery` into a const on the same tick. A stub installed inside a test would arrive
// after both.

import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { afterEach, beforeEach } from "bun:test";

GlobalRegistrator.register();

export type SentIpc = { channel: string; args: unknown[] };

// Every `sendIpc` the code under test issued, in order.
const sent: SentIpc[] = [];

// Resolvers of `__LUNAR_TEST_IPC_SENT__`, by the channel each waits for.
const waiting = new Map<string, Array<() => void>>();

(window as any).cefQuery = ({ request }: { request: string }) => {
	const { channel, args } = JSON.parse(request);
	sent.push({ channel, args });
	for (const resolve of waiting.get(channel) ?? []) resolve();
	waiting.delete(channel);
};

(globalThis as any).__LUNAR_TEST_IPC__ = sent;

// Settles once `channel` has been sent: at once if it already was, else at the call that sends it.
(globalThis as any).__LUNAR_TEST_IPC_SENT__ = (channel: string): Promise<void> =>
	sent.some((s) => s.channel === channel)
		? Promise.resolve()
		: new Promise((resolve) => waiting.set(channel, [...(waiting.get(channel) ?? []), resolve]));

// Puts the globals back after each test. The tests of a file share its global, and bun never calls
// a cleanup returned from `beforeEach`: a flag the code under test leaves on `window` is what the
// next test of the file runs against. A property is restored to the descriptor found in
// `beforeEach` and one added since is deleted; `Reflect` reports instead of throwing on one the
// code under test pinned as non-configurable (`window.luna`), which no test could undo anyway.
const watched: object[] = [globalThis, console, navigator];

type Shot = Map<PropertyKey, PropertyDescriptor>;

const photograph = (target: object): Shot => {
	const shot: Shot = new Map();
	for (const key of Reflect.ownKeys(target)) shot.set(key, Object.getOwnPropertyDescriptor(target, key)!);
	return shot;
};

const unchanged = (a: PropertyDescriptor, b: PropertyDescriptor) => Object.is(a.value, b.value) && a.get === b.get && a.set === b.set;

let found: Shot[] = [];

beforeEach(() => {
	found = watched.map(photograph);
});

afterEach(() => {
	watched.forEach((target, i) => {
		for (const key of Reflect.ownKeys(target)) {
			if (!found[i].has(key)) Reflect.deleteProperty(target, key);
		}
		for (const [key, was] of found[i]) {
			const now = Object.getOwnPropertyDescriptor(target, key);
			if (!now || !unchanged(was, now)) Reflect.defineProperty(target, key, was);
		}
	});
});
