// Tests `src/ui/early_runtime/cache_heal.js`, the Service Worker cache self-heal injected
// by `init_script`. It lived as an escaped string inside a Rust `format!` until now, which
// is why it had no coverage and why a wrong readiness criterion went unnoticed.
//
// The criterion is the point: the heal must NOT re-derive whether the host React capture
// is usable. `initModules` owns that verdict (it alone knows presence, validity and the
// CJS unwrap, for all three modules) and hands it over through `__lunaHostReady`.

import { afterAll, beforeEach, expect, test } from "bun:test";

const source = await Bun.file(new URL("../../src/ui/early_runtime/cache_heal.js", import.meta.url)).text();

const flush = () => new Promise((r) => setTimeout(r, 0));

type Harness = {
	deleted: string[];
	unregistered: { n: number };
	warned: string[];
	fallback: () => void;
};

const realWarn = console.warn;
afterAll(() => {
	console.warn = realWarn;
});

// Evaluate a fresh copy of the script over stubbed browser globals. `setTimeout` is
// swapped for the duration: the 35 s fallback timer is captured instead of armed.
function load(controller: boolean): Harness {
	const deleted: string[] = [];
	const unregistered = { n: 0 };
	const warned: string[] = [];
	let timed: () => void = () => {};

	console.warn = (...args: unknown[]) => void warned.push(args.join(" "));

	Object.defineProperty(navigator, "serviceWorker", {
		configurable: true,
		value: {
			controller: controller ? {} : null,
			getRegistrations: async () => [{ unregister: () => void unregistered.n++ }],
		},
	});
	const caches = {
		keys: async () => ["workbox-precache-v2"],
		delete: async (k: string) => void deleted.push(k),
	};
	(globalThis as any).caches = caches;
	(window as any).caches = caches;

	const realSetTimeout = globalThis.setTimeout;
	(globalThis as any).setTimeout = (fn: () => void) => {
		timed = fn;
		return 0;
	};
	try {
		new Function(source)();
	} finally {
		(globalThis as any).setTimeout = realSetTimeout;
	}
	return { deleted, unregistered, warned, fallback: () => timed() };
}

const report = (ok: boolean) => (globalThis as any).__lunaHostReady(ok);

beforeEach(() => {
	sessionStorage.clear();
	localStorage.clear();
	delete (globalThis as any).__lunaCreateRoot;
	delete (window as any).__lunaHostModules;
	delete (globalThis as any).__lunaHostReady;
});

test("a whole verdict clears the spent marker and never busts, even uncontrolled", async () => {
	// The session right after a bust runs uncontrolled, and it is the one that proves the
	// heal worked. Clearing must not sit behind the controller test.
	localStorage.setItem("__luna_heal_spent", "1");
	const h = load(false);
	report(true);
	await flush();
	expect(localStorage.getItem("__luna_heal_spent")).toBeNull();
	expect(h.deleted).toEqual([]);
});

test("an incomplete verdict busts once and records the attempt", async () => {
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual(["workbox-precache-v2"]);
	expect(h.unregistered.n).toBe(1);
	expect(localStorage.getItem("__luna_heal_spent")).toBe("1");
	expect(sessionStorage.getItem("__luna_react_heal")).toBe("1");
	// A bust drops every cache the origin has and only pays off next launch; it is never
	// allowed to happen silently.
	expect(h.warned.join("\n")).toContain("host React capture incomplete");
});

test("busts when react and the tag are present but the trio did not resolve", async () => {
	// THE REGRESSION. This is exactly the state the old two-boolean check read as healthy:
	// react captured, entry chunk tagged, but the react-dom/client capture never
	// registered; initModules demoted all three and nothing ever healed it.
	(window as any).__lunaHostModules = { react: { createElement() {}, useState() {} } };
	(globalThis as any).__lunaCreateRoot = () => {};
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual(["workbox-precache-v2"]);
	expect(localStorage.getItem("__luna_heal_spent")).toBe("1");
});

test("does not bust without a controlling worker", async () => {
	const h = load(false);
	report(false);
	await flush();
	expect(h.deleted).toEqual([]);
	expect(localStorage.getItem("__luna_heal_spent")).toBeNull();
});

test("does not bust a second time once the attempt is spent", async () => {
	localStorage.setItem("__luna_heal_spent", "1");
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual([]);
	// This is the only warning that a rewrite stopped matching TIDAL's bundle: without it
	// the app silently runs on bundled React forever and nobody finds out.
	expect(h.warned.join("\n")).toContain("no longer matches");
});

test("does not bust twice in one session", async () => {
	sessionStorage.setItem("__luna_react_heal", "1");
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual([]);
});

test("the fallback stays out of the way once a verdict arrived", async () => {
	const h = load(true);
	report(true);
	h.fallback();
	await flush();
	expect(h.deleted).toEqual([]);
});

test("the fallback busts on a raw capture miss when the bundle never reported", async () => {
	// initCore can stall indefinitely on a wedged worker; no verdict ever arrives. The
	// capture side effects land as TIDAL's own chunks execute and do not depend on it.
	const h = load(true);
	h.fallback();
	await flush();
	expect(h.deleted).toEqual(["workbox-precache-v2"]);
});

test("the fallback stays put when the raw capture looks whole", async () => {
	(window as any).__lunaHostModules = { react: {} };
	(globalThis as any).__lunaCreateRoot = () => {};
	const h = load(true);
	h.fallback();
	await flush();
	expect(h.deleted).toEqual([]);
});
