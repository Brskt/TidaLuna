// Tests for `handleReceiverMediaChanged` in `src/connect/middleware.ts`.
//
// Regression: handed to the delegate as a plain item, the receiver's own track was loaded again
// in this page's name whenever this page's quality self-loads it. Refused under the cast, that
// load raises the "controlled via another client" banner.

import { expect, mock, test } from "bun:test";
import { standInDelegate } from "./helpers/stand-in-delegate";

const track = {
	id: 77,
	title: "Cast track",
	duration: 200,
	album: { cover: "ab-cd", title: "Album" },
	artists: [{ name: "Artist" }],
};

// Only the fetch is replaced; `sendIpc` and the rest stay the real ones the preload wires up.
// `lookUp` answers it, and a test that needs to hold a lookup back replaces it.
const answer = (body: unknown) => ({ status: 200, json: async () => body });
let lookUp = async (_url: string): Promise<unknown> => answer(track);
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	tidalFetch: (url: string) => lookUp(url),
}));

const { handleReceiverMediaChanged, followCastQueue } = await import("../src/connect/middleware");

standInDelegate();

test("the receiver's track reaches the delegate as one already playing", async () => {
	const calls: unknown[][] = [];
	(window as any).nativeInterface = { playback: { setCurrentMediaItem: (...args: unknown[]) => calls.push(args) } };
	const store = { dispatch: () => {}, getState: () => ({ session: { countryCode: "FR" }, playbackControls: {} }) };

	await handleReceiverMediaChanged(store, { mediaId: "77" } as any);

	expect(calls).toHaveLength(1);
	expect(calls[0][0]).toMatchObject({ productId: 77, type: "track" });
	expect(calls[0][1]).toEqual({ alreadyPlaying: true });
});

// Regression: announced alone, the cast's track never reached TIDAL's player, and a Play after
// the cast reloaded the track it held before (TIDAL's `maybeHardReload`).

type Action = { type: string; payload?: any };

// Records, in one log, what the middleware dispatches and announces. `prefilled` is what TIDAL's
// own sagas leave behind a reset queue: its load of the queue's new item prefills the bar first.
const recording = ({ prefilled = true, queue }: { prefilled?: boolean; queue?: unknown } = {}) => {
	const log: Action[] = [];
	const store = {
		dispatch: (a: Action) => void log.push(a),
		getState: () => ({ session: { countryCode: "FR" }, playbackControls: { prefilled }, playQueue: queue }),
	};
	return { log, store };
};
const announce = async (store: unknown, mediaId = "77") => {
	await handleReceiverMediaChanged(store as any, { mediaId } as any);
};
const withDelegate = async (log: Action[], run: () => Promise<void>) => {
	(window as any).nativeInterface = {
		playback: { setCurrentMediaItem: (item: any) => void log.push({ type: "announced", payload: item.productId }) },
	};
	await run();
};
const typesOf = (log: Action[]) => log.map((a) => a.type);
const element = (mediaItemId: number) => ({
	context: { type: "UNKNOWN" },
	mediaItemId,
	priority: "priority_none",
	uid: "pq_0",
});

test("the receiver's track becomes this page's play queue, as TIDAL's receiver makes it", async () => {
	const { log, store } = recording();
	await announce(store);

	expect(log.filter((a) => a.type === "playQueue/RESET")).toEqual([
		{
			type: "playQueue/RESET",
			payload: { currentIndex: 0, elements: [element(77)], originalSource: [element(77)] },
		},
	]);
});

test("the announcement leaves the media product to TIDAL's own transition", async () => {
	// The hand-made one grafted the cast's id onto the product TIDAL's player still held.
	const { log, store } = recording();
	await announce(store);

	expect(typesOf(log)).not.toContain("playbackControls/MEDIA_PRODUCT_TRANSITION");
});

test("the catalog holds the track and the delegate knows it plays before the queue names it", async () => {
	const { log, store } = recording();
	await withDelegate(log, () => announce(store));

	const types = typesOf(log);
	expect(types.indexOf("content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS")).toBeGreaterThanOrEqual(0);
	expect(types.indexOf("content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS")).toBeLessThan(types.indexOf("playQueue/RESET"));
	expect(types.indexOf("announced")).toBeGreaterThanOrEqual(0);
	expect(types.indexOf("announced")).toBeLessThan(types.indexOf("playQueue/RESET"));
});

test("a queue TIDAL does not load is loaded by its own reload of the queue's item", async () => {
	const { log, store } = recording({ prefilled: false });
	await announce(store);

	expect(typesOf(log).slice(-2)).toEqual(["playQueue/RESET", "playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE"]);
});

test("a queue TIDAL loads is not loaded a second time", async () => {
	const { log, store } = recording({ prefilled: true });
	await announce(store);

	expect(typesOf(log)).not.toContain("playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE");
});

test("an announcement overtaken by a later one changes nothing", async () => {
	// The phone skips twice in quick succession, and the first track's lookup lands last.
	const held = new Map<string, (body: unknown) => void>();
	lookUp = (url: string) =>
		new Promise((resolve) => held.set(url.match(/tracks\/(\d+)/)![1], (body) => resolve(answer(body))));
	const { log, store } = recording();
	try {
		const first = announce(store, "5");
		const second = announce(store, "6");
		held.get("6")!({ ...track, id: 6 });
		await second;
		held.get("5")!({ ...track, id: 5 });
		await first;
	} finally {
		lookUp = async () => answer(track);
	}

	expect(log.filter((a) => a.type === "playQueue/RESET").map((a) => a.payload.elements[0].mediaItemId)).toEqual([6]);
	expect(log.filter((a) => a.type === "content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS")).toHaveLength(1);
});

test("a refused load of another track reloads the queue's", () => {
	const { log, store } = recording({ queue: { currentIndex: 0, elements: [{ mediaItemId: 78 }] } });
	followCastQueue(store as any, "77");

	expect(typesOf(log)).toEqual(["playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE"]);
});

test("a refused load of the queue's own track reloads nothing", () => {
	const { log, store } = recording({ queue: { currentIndex: 0, elements: [{ mediaItemId: 78 }] } });
	followCastQueue(store as any, "78");

	expect(log).toEqual([]);
});
