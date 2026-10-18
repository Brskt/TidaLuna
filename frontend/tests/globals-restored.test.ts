// Tests the restore `setup.ts` runs after every test. The two tests share one process: the second
// checks what the first left behind.

import { expect, test } from "bun:test";

const realWarn = console.warn;
const realFetch = globalThis.fetch;
const realDelegate = (window as any).nativeInterface;
const realWorker = Object.getOwnPropertyDescriptor(navigator, "serviceWorker");

test("a test replaces and adds shared globals and leaves them", () => {
	console.warn = () => {};
	(globalThis as any).fetch = () => Promise.resolve(new Response("stub"));
	(window as any).nativeInterface = { playback: {} };
	(window as any).__LUNAR_STUB_LEFT_BEHIND__ = 1;
	Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: {} });
});

test("the next test finds them as they were before", () => {
	expect(console.warn).toBe(realWarn);
	expect(globalThis.fetch).toBe(realFetch);
	expect((window as any).nativeInterface).toBe(realDelegate);
	expect("__LUNAR_STUB_LEFT_BEHIND__" in window).toBe(false);
	expect(Object.getOwnPropertyDescriptor(navigator, "serviceWorker")).toEqual(realWorker);
});
