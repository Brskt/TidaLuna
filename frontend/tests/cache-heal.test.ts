// Tests `early-runtime/cache_heal.js`, the Service Worker cache self-heal injected
// by `init_script`, by running the shipped source.
//
// The criterion is the point: the heal must NOT re-derive whether the host React capture
// is usable. `initModules` owns that verdict (it alone knows presence, validity and the
// CJS unwrap, for all three modules) and hands it over through `__lunaHostReady`.

import { afterAll, beforeEach, expect, test } from "bun:test";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("cache_heal");

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
		runFragment(source);
	} finally {
		(globalThis as any).setTimeout = realSetTimeout;
	}
	return { deleted, unregistered, warned, fallback: () => timed() };
}

const report = (ok: boolean) => (globalThis as any).__lunaHostReady(ok);

const reset = () => {
	sessionStorage.clear();
	localStorage.clear();
	delete (globalThis as any).__lunaCreateRoot;
	delete (window as any).__lunaHostModules;
	delete (globalThis as any).__lunaHostReady;
	delete (self as any).__lunaRewriteEpoch;
};

beforeEach(reset);

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
	expect(sessionStorage.getItem("__luna_busted")).toBe("1");
	// A bust drops every cache the origin has and only pays off next launch; it is never
	// allowed to happen silently.
	expect(h.warned.join("\n")).toContain("host React capture incomplete");
});

test("busts when react and the tag are present but the trio did not resolve", async () => {
	// THE REGRESSION. This is exactly the state the old two-boolean check read as healthy:
	// react captured, createRoot tagged, but the react-dom/client capture never
	// registered; initModules demoted all three and nothing ever healed it.
	(window as any).__lunaHostModules = { react: { createElement() {}, useState() {} } };
	(globalThis as any).__lunaCreateRoot = () => {};
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual(["workbox-precache-v2"]);
	expect(localStorage.getItem("__luna_heal_spent")).toBe("1");
});

// The spent warning is the only sign that a rewrite stopped matching TIDAL's bundle: without it
// the app silently runs on bundled React forever and nobody finds out.
test("does not bust without a controller, once the attempt is spent, or twice in one session", async () => {
	for (const { what, controller, local, session, spent, warns } of [
		{ what: "no controlling worker", controller: false, spent: null },
		{ what: "attempt already spent", controller: true, local: "1", spent: "1", warns: "no longer matches" },
		{ what: "already busted this session", controller: true, session: "1", spent: null },
	]) {
		reset();
		if (local) localStorage.setItem("__luna_heal_spent", local);
		if (session) sessionStorage.setItem("__luna_busted", session);
		const h = load(controller);
		report(false);
		await flush();
		expect(h.deleted, what).toEqual([]);
		expect(localStorage.getItem("__luna_heal_spent"), what).toBe(spent);
		if (warns) expect(h.warned.join("\n"), what).toContain(warns);
	}
});

test("a verdict in the session an epoch bust condemned judges nothing", async () => {
	// The page keeps running the chunks the old cache served it: an incomplete capture says
	// nothing about the current rewrites, whatever an earlier launch left behind.
	(self as any).__lunaRewriteEpoch = "new";
	localStorage.setItem("__luna_rewrite_epoch", "old");
	localStorage.setItem("__luna_heal_spent", "1");
	const h = load(true);
	report(false);
	await flush();
	expect(h.unregistered.n).toBe(1);
	expect(h.warned.join("\n")).not.toContain("still incomplete");
});

test("an epoch bust spends the one bust a stale cache needs", async () => {
	// That bust refetched everything through the current rewrites: a later miss means a
	// pattern stopped matching, and a second bust would only repeat the first.
	(self as any).__lunaRewriteEpoch = "new";
	localStorage.setItem("__luna_rewrite_epoch", "old");
	load(true);
	await flush();
	sessionStorage.clear();
	const h = load(true);
	report(false);
	await flush();
	expect(h.deleted).toEqual([]);
	expect(h.warned.join("\n")).toContain("no longer matches");
});

// initCore can stall indefinitely on a wedged worker and no verdict ever arrives. The capture
// side effects land as TIDAL's own chunks execute and do not depend on it.
test("the fallback busts only when no verdict arrived and the raw capture is incomplete", async () => {
	for (const { what, verdict, capture, deleted } of [
		{ what: "a verdict arrived", verdict: true, capture: false, deleted: [] },
		{ what: "no verdict, capture missing", verdict: false, capture: false, deleted: ["workbox-precache-v2"] },
		{ what: "no verdict, capture whole", verdict: false, capture: true, deleted: [] },
	]) {
		reset();
		if (capture) {
			(window as any).__lunaHostModules = { react: {} };
			(globalThis as any).__lunaCreateRoot = () => {};
		}
		const h = load(true);
		if (verdict) report(true);
		h.fallback();
		await flush();
		expect(h.deleted, what).toEqual(deleted);
	}
});
