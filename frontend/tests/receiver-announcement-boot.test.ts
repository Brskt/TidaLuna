// Tests `handleReceiverMediaChanged` (`src/connect/middleware.ts`) for an announcement that reaches
// the page before TIDAL's store is published.
//
// Regression: a phone casting during boot is announced by Rust to a page whose store is the lib's
// proxy, which throws on its first use until the store is published. The announcement was dropped
// whole, with nothing to say so, and the page showed nothing of the track until the next one.

import { afterEach, expect, mock, spyOn, test } from "bun:test";
import { StoreNotPublishedError } from "../plugins/lib/src/redux/storeNotPublished";

const track = { id: 77, title: "Cast track", duration: 200, album: { cover: "ab-cd", title: "Album" }, artists: [{ name: "Artist" }] };

let lookUp = async (_url: string): Promise<unknown> => ({ status: 200, json: async () => track });
const fetched: string[] = [];
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	tidalFetch: (url: string) => {
		fetched.push(url);
		return lookUp(url);
	},
}));

const { handleReceiverMediaChanged } = await import("../src/connect/middleware");

type Action = { type: string; payload?: any };

// The lib's proxy: any use throws until `publish()`.
const lateStore = () => {
	let there = false;
	const log: Action[] = [];
	const store = {
		dispatch: (a: Action) => {
			if (!there) throw new StoreNotPublishedError();
			log.push(a);
		},
		getState: () => {
			if (!there) throw new StoreNotPublishedError();
			return { session: { countryCode: "FR" }, playbackControls: { prefilled: true } };
		},
	};
	let release!: () => void;
	const ready = new Promise<void>((r) => (release = r));
	return { log, store, ready: () => ready, publish: () => ((there = true), release()) };
};
const settled = () => new Promise((r) => setTimeout(r, 0));
const announce = (store: unknown, ready: () => Promise<void>, mediaId = "77") =>
	handleReceiverMediaChanged(store as any, { mediaId } as any, ready);

afterEach(() => {
	fetched.length = 0;
	lookUp = async () => ({ status: 200, json: async () => track });
});

test("an announcement that reaches the page before its store is applied once the store is there", async () => {
	const { log, store, ready, publish } = lateStore();
	const announced: unknown[] = [];
	(window as any).nativeInterface = { playback: { setCurrentMediaItem: (item: any) => announced.push(item.productId) } };

	const done = announce(store, ready);
	await settled();
	expect(fetched).toEqual([]);
	expect(announced).toEqual([]);

	publish();
	await done;

	expect(announced).toEqual([77]);
	expect(log.map((a) => a.type)).toEqual(["content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS", "playQueue/RESET"]);
});

test("an announcement overtaken while it waits for the store changes nothing", async () => {
	const { log, store, ready, publish } = lateStore();
	(window as any).nativeInterface = { playback: { setCurrentMediaItem: () => {} } };
	lookUp = async (url: string) => {
		const id = Number(url.match(/tracks\/(\d+)/)![1]);
		return { status: 200, json: async () => ({ ...track, id }) };
	};

	const first = announce(store, ready, "5");
	const second = announce(store, ready, "6");
	await settled();
	publish();
	await Promise.all([first, second]);

	expect(fetched).toHaveLength(1);
	expect(fetched[0]).toContain("/tracks/6");
	expect(log.filter((a) => a.type === "playQueue/RESET").map((a) => a.payload.elements[0].mediaItemId)).toEqual([6]);
});

test("a lookup that fails after the wait stays silent", async () => {
	const { log, store, ready, publish } = lateStore();
	lookUp = () => Promise.reject(new Error("offline"));
	const warn = spyOn(console, "warn").mockImplementation(() => {});
	const error = spyOn(console, "error").mockImplementation(() => {});
	try {
		const done = announce(store, ready);
		await settled();
		publish();
		await done;
		expect(log).toEqual([]);
		expect(warn).not.toHaveBeenCalled();
		expect(error).not.toHaveBeenCalled();
	} finally {
		warn.mockRestore();
		error.mockRestore();
	}
});
