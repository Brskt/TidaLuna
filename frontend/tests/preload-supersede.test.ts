// What TIDAL's PRELOAD_ITEM (its `setNext`) does to the next track the SDK's native player staged in Rust.
//
// Regression: the SDK unloads the native player's preload only when the new next goes to that player; a
// next served by Shaka (or a video) left the staged track in Rust, which played it when the current one
// ended in place of the one TIDAL chose. The page unloads it with the SDK's own unset, never with TIDAL's
// PRELOAD_ITEM(undefined): that saga's PRELOAD_SUCCESS releases the fallback TIDAL keeps for a preload that
// did not finish.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import { nameStream } from "./helpers/sdk-answer";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

let raw: Dispatch[] = [];
let dispatched: Dispatch[] = [];
// What the SDK's `setNext(undefined)` does in each test: the native player's unload, by default.
let unsetting: (() => Promise<unknown>) | null = null;
let unsets: unknown[] = [];
// The order the SDK's unset and the put-again happened in.
let order: string[] = [];
let sdkAvailable = true;
let delegate: any;

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({
			playQueue: { currentIndex: 0, repeatMode: 0, elements: [] },
			playbackControls: { mediaProduct: { productId: "111", referenceId: "r111" } },
			content: { mediaItems: {} },
		}),
		dispatch: (a: Dispatch) => dispatched.push(a),
		subscribe: () => () => {},
		// `patchDispatch`'s unintercepted channel: the one the put-again goes through.
		__lunaRawDispatch: (a: Dispatch) => {
			order.push("put again");
			raw.push(a);
		},
	},
}));

// The page's way to the SDK (`src/sdk-player.ts`, tested apart): a stand-in whose unset unloads the native player
// the way `NativePlayer.unloadPreloadedMediaProduct` does, through the delegate.
mock.module("../src/sdk-player", () => ({
	startSdkPlayer: async () => {},
	sdkPlayer: () =>
		sdkAvailable
			? {
					setNext: (product?: unknown) => {
						unsets.push(product);
						order.push("sdk unset");
						delegate.cancelPreload();
						return unsetting ? unsetting() : Promise.resolve();
					},
				}
			: null,
}));

bootStubs();

await import("../src/index");
const { interceptors, putInterceptors } = await import("../render/src/exposeTidalInternals.patchAction");
const { leavesStagedBehind, nextProductOf, unloadBeforeNext, PRELOAD_ITEM } = await import("../src/controllers/player");

// `init` registers the hooks once its awaits are through, which an import does not wait for.
for (let i = 0; i < 2000 && !((putInterceptors[PRELOAD_ITEM]?.size ?? 0) > 0 && (interceptors[PRELOAD_ITEM]?.size ?? 0) > 0); i++) await Bun.sleep(1);

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const channel = (name: string) => sent.filter((s) => s.channel === name);

const product = (productId: string, productType = "track") => ({ productId, productType, sourceId: "s", sourceType: "ALBUM", referenceId: `r${productId}` });
const again = (productId: string, productType = "track") => ({ type: PRELOAD_ITEM, payload: product(productId, productType) });
// Turns the microtasks the unset's settling takes.
const settled = () => Bun.sleep(0);

const stage = async (productId: string) => {
	await nameStream(`https://cdn/${productId}.flac`, productId);
	delegate.preload(`https://cdn/${productId}.flac`, "flac", "");
};
type Rail = "put" | "dispatch";
const registry = (rail: Rail) => (rail === "put" ? putInterceptors : interceptors);
// What the registry's runner does with a hook's answer: a `true` drops the action. A payload-less action is handed over whole.
const preloadItem = (rail: Rail, payload: unknown): boolean => {
	let cancelled = false;
	for (const cb of registry(rail)[PRELOAD_ITEM] ?? []) {
		if (cb(payload !== undefined ? payload : { type: PRELOAD_ITEM, payload }, PRELOAD_ITEM) === true) cancelled = true;
	}
	return cancelled;
};

beforeEach(async () => {
	sdkAvailable = true;
	unsetting = null;
	delegate = (window as any).NativePlayerComponent.Player();
	// Nothing may stay staged from the test before, nor any unset still running (the component is the page's one).
	delegate.cancelPreload();
	await settled();
	raw = [];
	dispatched = [];
	unsets = [];
	order = [];
	sent.length = 0;
});

test("the rule: a staged track is left behind by a next that names another product, and by nothing else", () => {
	expect([
		leavesStagedBehind("112", "333"),
		leavesStagedBehind("112", "112"),
		leavesStagedBehind("112", null),
		leavesStagedBehind(null, "333"),
		leavesStagedBehind(null, null),
	]).toEqual([true, false, false, false, false]);
});

test("a product is named by its id, and an unset names none", () => {
	expect([nextProductOf(product("333")), nextProductOf({ productId: 7 }), nextProductOf(undefined), nextProductOf({ type: PRELOAD_ITEM }), nextProductOf({ productId: {} })]).toEqual([
		"333",
		"7",
		null,
		null,
		null,
	]);
});

test("TIDAL's PRELOAD_ITEM(undefined) is never dispatched", async () => {
	await stage("112");
	preloadItem("put", product("333"));
	await settled();

	expect([...raw, ...dispatched].filter((a) => a.type === PRELOAD_ITEM && a.payload === undefined)).toEqual([]);
});

test("the unset itself is the SDK's to handle", async () => {
	await stage("112");

	expect(preloadItem("put", undefined)).toBe(false);
	await settled();
	expect(unsets).toStrictEqual([]);
});

test("a video next, which the SDK never preloads, leaves the staged track behind too", async () => {
	await stage("112");

	expect(preloadItem("put", product("333", "video"))).toBe(true);
	await settled();
	expect(raw).toEqual([again("333", "video")]);
});

test("the page asks Rust for no cancel itself: the SDK's own unload does, and clears the record", async () => {
	await stage("112");
	preloadItem("put", product("333"));

	// The stand-in's unset went through the delegate, as the SDK's native player does.
	expect(channel("player.preload.cancel")).toHaveLength(1);
	await settled();
	expect(preloadItem("put", product("333"))).toBe(false);
});

test("a next asked for while the unset runs waits for it, and only the latest goes through", async () => {
	let release!: () => void;
	unsetting = () => new Promise<void>((r) => (release = r));
	await stage("112");

	expect(preloadItem("put", product("333"))).toBe(true);
	expect(preloadItem("put", product("333"))).toBe(true);
	expect(preloadItem("dispatch", product("444"))).toBe(true);
	expect(raw).toEqual([]);
	release();
	await settled();

	expect(unsets).toStrictEqual([undefined]);
	expect(raw).toEqual([again("444")]);
});

test("an unset asked for while the unset runs is that unset: nothing is put again", async () => {
	let release!: () => void;
	unsetting = () => new Promise<void>((r) => (release = r));
	await stage("112");

	preloadItem("put", product("333"));
	expect(preloadItem("put", undefined)).toBe(true);
	release();
	await settled();

	expect(raw).toEqual([]);
});

test("an unset that rejects still puts the next again, and says so", async () => {
	unsetting = () => Promise.reject(new Error("unload failed"));
	await stage("112");
	const errors: unknown[][] = [];
	const original = console.error;
	console.error = (...args: unknown[]) => void errors.push(args);
	try {
		preloadItem("put", product("333"));
		await settled();
	} finally {
		console.error = original;
	}

	expect(raw).toEqual([again("333")]);
	expect(errors).toHaveLength(1);
});

test("an unset that throws still puts the next again", async () => {
	unsetting = () => {
		throw new Error("unload threw");
	};
	await stage("112");
	const original = console.error;
	console.error = () => {};
	try {
		preloadItem("put", product("333"));
		await settled();
	} finally {
		console.error = original;
	}

	expect(raw).toEqual([again("333")]);
});

test("without the SDK's API the action is left be", async () => {
	sdkAvailable = false;
	await stage("112");

	expect(preloadItem("put", product("333"))).toBe(false);
	await settled();
	expect(raw).toEqual([]);
});

test("without the unintercepted channel the action is left be", async () => {
	const hook = unloadBeforeNext(
		() => ({ supersedesStaged: () => true }),
		() => ({ setNext: () => Promise.resolve() }),
		() => undefined,
	);

	expect(hook(product("333"))).toBe(false);
});

test("a stream no playbackinfo answer named cannot be compared, and is left alone", async () => {
	const original = console.warn;
	console.warn = () => {};
	try {
		delegate.preload("https://cdn/never-seen.flac", "flac", "");
	} finally {
		console.warn = original;
	}

	expect(preloadItem("put", product("333"))).toBe(false);
	expect(unsets).toStrictEqual([]);
});

test("Rust takes its staged slot at every completed: the next PRELOAD_ITEM leaves nothing behind", async () => {
	await stage("112");
	(window as any).NativePlayerComponent.trigger("mediastate", "completed");

	expect(preloadItem("put", product("333"))).toBe(false);
	expect(unsets).toStrictEqual([]);
});

test("the load that takes the staged stream ends its record", async () => {
	await stage("112");
	delegate.load("https://cdn/112.flac", "flac", "");

	expect(preloadItem("put", product("333"))).toBe(false);
});

test("any other load leaves it: the SDK's native player keeps its own record of the preload", async () => {
	// With another player active the native player has no track to reset: only an unset clears its preload.
	await stage("112");
	delegate.load("https://cdn/113.flac", "flac", "");

	expect(preloadItem("put", product("333"))).toBe(true);
	await settled();
});

test("the record follows the latest preload", async () => {
	await stage("112");
	await stage("113");

	expect(preloadItem("put", product("112"))).toBe(true);
	await settled();
	await stage("113");
	expect(preloadItem("put", product("113"))).toBe(false);
});
