// What the SDK's native-player delegate (`src/controllers/player.ts`) tells Rust about the track it
// preloaded and the one that plays while a self-loaded track does.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { nameStream } from "./helpers/sdk-answer";
import type { SentIpc } from "./setup";

let queue = { currentIndex: 0, repeatMode: 0, elements: [1, 2, 3].map((id) => ({ mediaItemId: id, uid: `pq_${id}` })) };
let mediaProduct: { productId: number; referenceId: string } = { productId: 1, referenceId: "pq_1" };

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: queue, playbackControls: { mediaProduct }, content: { mediaItems: {} } }),
		dispatch: () => {},
		subscribe: () => () => {},
	},
}));

const { createNativePlayerComponent } = await import("../src/controllers/player");
const { setSelfLoad } = await import("../src/audio-proxy");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const channel = (name: string) => sent.filter((s) => s.channel === name);
// The arguments of the SDK's native load, as Rust reads them: restart at 3, product at 5.
const loadOf = (name: string) => channel(name).at(-1)!.args;

let player: any;

beforeEach(() => {
	sent.length = 0;
	queue = { currentIndex: 0, repeatMode: 0, elements: queue.elements };
	mediaProduct = { productId: 1, referenceId: "pq_1" };
	player = createNativePlayerComponent().Player();
});

afterEach(() => setSelfLoad(false));

test("the load that switches to the preloaded stream names the preloaded track, not the one the store still holds", async () => {
	// TIDAL's switch from Shaka to the player that preloaded: the store names the track that just ended.
	await nameStream("https://cdn/named-2.flac", "2");
	player.load("https://cdn/1.flac", "flac", "");
	mediaProduct = { productId: 3, referenceId: "pq_3" };
	player.preload("https://cdn/named-2.flac", "flac", "");
	player.load("https://cdn/named-2.flac", "flac", "");

	expect(loadOf("player.load")[5]).toBe("2");
});

test("a stream that was not preloaded keeps the store's product and its restart rule", () => {
	player.load("https://cdn/1.flac", "flac", "");
	mediaProduct = { productId: 2, referenceId: "pq_2" };
	player.preload("https://cdn/9.flac", "flac", "");
	player.load("https://cdn/other.flac", "flac", "");

	expect([loadOf("player.load")[3], loadOf("player.load")[5]]).toEqual([true, 2]);
});

// The queue and the store are what the page can read of TIDAL's state, and each is stale somewhere: the queue
// has moved on when the SDK's `next()` reaches its delegate, the store has not moved yet when the SDK switches
// to a preloaded stream. The stream is the one thing the SDK's call carries, and the SDK's own playbackinfo
// answer names its track.

test("a preload names the track of its stream, whatever the queue's next entry is", async () => {
	// The queue wrapped to the first entry while the SDK was preloading the entry it moved to.
	queue = { currentIndex: 2, repeatMode: 1, elements: queue.elements };
	await nameStream("https://cdn/3.flac", "3");
	player.preload("https://cdn/3.flac", "flac", "");
	queue = { currentIndex: 1, repeatMode: 2, elements: queue.elements };
	await nameStream("https://cdn/7.flac", "7");
	player.preload("https://cdn/7.flac", "flac", "");

	expect(channel("player.preload").map((s) => s.args[3])).toEqual(["3", "7"]);
});

test("a stream no playbackinfo answer named is sent untagged by a preload, and keeps the store's product at a load", async () => {
	const warned: unknown[][] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void warned.push(args);
	try {
		player.preload("https://cdn/never-seen.flac", "flac", "");
	} finally {
		console.warn = original;
	}
	player.load("https://cdn/other-never-seen.flac", "flac", "");

	expect(channel("player.preload")[0]!.args[3]).toBe("");
	expect(warned).toHaveLength(1);
	expect(loadOf("player.load")[5]).toBe(1);
});
