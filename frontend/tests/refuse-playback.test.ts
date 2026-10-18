// Tests for `src/refuse-playback.ts`: a store that is not published yet is no failure, every other
// failure is raised, and no step is lost to another's.
//
// Regression: the module's guard for "no store yet" was a `try` around the `require` of the store
// module, which never throws. The store is a lazy proxy whose `get` throws: the exception came out
// of `store.dispatch`, past the caller, and aborted the rest of the bridge's batch.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { StoreNotPublishedError, isStoreNotPublished } from "../plugins/lib/src/redux/storeNotPublished";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let triggered: { name: string; target: unknown }[] = [];
// What reading the store throws: nothing once it is published.
let storeFault: Error | null = null;
let dispatchFault: { type: string; error: Error } | null = null;
let triggerFault: Error | null = null;

// The contract of `plugins/lib/src/redux/store.ts` (its `get` trap): reading anything from the
// store before it is published throws a `StoreNotPublishedError`, while requiring the module never does.
const storeStub = () => ({
	store: new Proxy({} as Record<string, unknown>, {
		get(_, prop) {
			if (storeFault) throw storeFault;
			if (prop === "dispatch") {
				return (a: Dispatch) => {
					if (dispatchFault && a.type === dispatchFault.type) throw dispatchFault.error;
					dispatched.push(a);
				};
			}
			return undefined;
		},
	}),
});
mock.module("../plugins/lib/src/redux/store", storeStub);

const { refusePlayback } = await import("../src/refuse-playback");

const refuse = () => refusePlayback("tidalunar_test", "why", "the banner text");
const all = [
	"playbackControls/STOP",
	"playbackControls/SET_PLAYBACK_STATE",
	"message/MESSAGE_ERROR",
];

let warned: unknown[][] = [];
const realWarn = console.warn;

beforeEach(() => {
	dispatched = [];
	triggered = [];
	warned = [];
	storeFault = null;
	dispatchFault = null;
	triggerFault = null;
	mock.module("../plugins/lib/src/redux/store", storeStub);
	(window as any).NativePlayerComponent = {
		trigger: (name: string, target: unknown) => {
			triggered.push({ name, target });
			if (triggerFault) throw triggerFault;
		},
	};
	console.warn = (...args: unknown[]) => void warned.push(args);
});

afterEach(() => {
	console.warn = realWarn;
});

test("a refusal stops the track, clears the stall and banners, in that order", () => {
	refuse();

	expect(triggered).toEqual([{ name: "mediaerror", target: { error: "why", errorCode: "tidalunar_test" } }]);
	expect(dispatched.map((d) => d.type)).toEqual(all);
	expect(dispatched[1]).toEqual({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" });
	expect(warned).toEqual([]);
});

test("a refusal before the store is published does not throw, still raises the error on the element, and says so", () => {
	storeFault = new StoreNotPublishedError();

	expect(refuse).not.toThrow();

	expect(triggered).toEqual([{ name: "mediaerror", target: { error: "why", errorCode: "tidalunar_test" } }]);
	expect(dispatched).toEqual([]);
	expect(warned.length).toBe(1);
	expect(String(warned[0]?.[0])).toContain("refusePlayback");
});

test("the store steps dropped for want of the store are not replayed once it appears", async () => {
	storeFault = new StoreNotPublishedError();
	refuse();
	storeFault = null;
	await new Promise((r) => setTimeout(r, 0));

	// A late STOP would land on whatever the listener started since.
	expect(dispatched).toEqual([]);
});

test("a store that throws for another reason is not taken for an unpublished one", () => {
	storeFault = new Error("middleware exploded");

	// One failure per step that needed the store.
	expect(refuse).toThrow("several steps failed");

	expect(warned).toEqual([]);
	expect(triggered.map((t) => t.name)).toEqual(["mediaerror"]);
});

test("a reducer that throws is raised, after the steps behind it ran", () => {
	dispatchFault = { type: "playbackControls/STOP", error: new Error("reducer exploded") };

	expect(refuse).toThrow("reducer exploded");

	expect(triggered.map((t) => t.name)).toEqual(["mediaerror"]);
	expect(dispatched.map((d) => d.type)).toEqual(all.slice(1));
	expect(warned).toEqual([]);
});

test("two steps that fail are raised together, neither lost", () => {
	triggerFault = new Error("listener exploded");
	dispatchFault = { type: "playbackControls/STOP", error: new Error("reducer exploded") };

	let raised: unknown;
	try {
		refuse();
	} catch (e) {
		raised = e;
	}

	expect(raised).toBeInstanceOf(AggregateError);
	expect((raised as AggregateError).errors.map((e: Error) => e.message)).toEqual(["listener exploded", "reducer exploded"]);
});

test("a page without the player bridge still gets its store steps", () => {
	(window as any).NativePlayerComponent = undefined;

	expect(refuse).not.toThrow();

	expect(dispatched.map((d) => d.type)).toEqual(all);
});

test("the unpublished store is recognised by its type: not by its message, and across bundles", () => {
	expect(isStoreNotPublished(new StoreNotPublishedError())).toBe(true);
	// Another bundle's copy of the class.
	expect(isStoreNotPublished(Object.assign(new Error("x"), { name: "StoreNotPublishedError" }))).toBe(true);
	expect(isStoreNotPublished(new Error(new StoreNotPublishedError().message))).toBe(false);
	expect(isStoreNotPublished("StoreNotPublishedError")).toBe(false);
});
