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

(window as any).cefQuery = ({ request }: { request: string }) => {
	const { channel, args } = JSON.parse(request);
	sent.push({ channel, args });
};

(globalThis as any).__LUNAR_TEST_IPC__ = sent;

// Puts the shared globals back after each test. Bun runs every file of a run in one process and
// never calls a cleanup returned from `beforeEach`: a stub a test leaves behind is what the code
// of every file after it runs against. A property is restored to the descriptor found in
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
