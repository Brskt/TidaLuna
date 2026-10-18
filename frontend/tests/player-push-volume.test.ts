// Tests for the `volume` event of the batch in `src/index.ts` (`__TIDALUNAR_PLAYER_PUSH__`): the
// OS mixer's level is mirrored whole, and a failure of the mirror is reported, never swallowed.
//
// Regression: the branch's empty `catch` took every error for "store not published yet". A store
// subscriber that threw on the first of the three writes cut the other two short and kept the
// report from being taken in. TIDAL did not follow the mixer and the page's next levels
// carried a `seen` older than Rust's, which Rust drops.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";
import { StoreNotPublishedError } from "../plugins/lib/src/redux/storeNotPublished";
import { FakeRust } from "./helpers/rust-level";
import type { SentIpc } from "./setup";

type Dispatch = { type: string; payload?: unknown };

let committed: Dispatch[] = [];
// Throws once the reducer of a write has committed it, as a plugin's subscriber does.
let subscriberThrowsOn = new Set<string>();
let storePublished = true;

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => {
			if (!storePublished) throw new StoreNotPublishedError();
			committed.push(a);
			if (subscriberThrowsOn.has(a.type)) throw new Error(`subscriber of ${a.type} exploded`);
		},
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

bootStubs();

await import("../src/index");
const { applyVolume } = await import("../src/volume");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);
const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;

const UNMUTE = "playbackControls/SET_VOLUME_UNMUTE";
const MUTE = "playbackControlsActions/INDICATE_MUTE";
const VOLUME = "playbackControls/SET_VOLUME";

let rust = new FakeRust();
let reported: unknown[][] = [];
let warned = 0;

beforeEach(() => {
	committed = [];
	subscriberThrowsOn = new Set();
	storePublished = true;
	rust = new FakeRust();
	reported = [];
	warned = 0;
	sent.length = 0;
	mock.module("../plugins/lib/src/redux/store", storeStub);
	(window as any).NativePlayerComponent = { trigger: () => {} };
	console.error = (...args: unknown[]) => void reported.push(args);
	console.warn = () => void warned++;
});

test("the failure is reported with the event it came from", () => {
	subscriberThrowsOn.add(UNMUTE);

	push([{ t: "volume", v: rust.remote(40, 40) }]);

	expect(reported.length).toBe(1);
	expect(String(reported[0]?.[0])).toContain("[bridge]");
	expect(reported[0]?.[1]).toBe("volume");
	expect((reported[0]?.[2] as Error).message).toBe(`subscriber of ${UNMUTE} exploded`);
});

test("several failing writes are reported together", () => {
	subscriberThrowsOn.add(UNMUTE).add(VOLUME);

	push([{ t: "volume", v: rust.remote(40, 40) }]);

	expect(committed.map((d) => d.type)).toEqual([UNMUTE, MUTE, VOLUME]);
	expect(reported.length).toBe(1);
	expect((reported[0]?.[2] as AggregateError).errors.length).toBe(2);
});

test("after a failing write the page's next level still reaches Rust", () => {
	subscriberThrowsOn.add(UNMUTE);

	push([{ t: "volume", v: rust.remote(40, 40) }]);
	applyVolume(55);
	rust.applies(sent);

	expect(rust.volume).toBe(55);
});

test("a store not published yet is no failure, and the report is not taken in", () => {
	storePublished = false;
	const report = rust.remote(40, 40);

	push([{ t: "volume", v: report }]);

	expect(committed).toEqual([]);
	expect(reported).toEqual([]);
	expect(warned).toBe(0);
	// Not taken in: the page's level is still refused, and the same report mirrors in full once found.
	applyVolume(55);
	rust.applies(sent);
	expect(rust.volume).toBe(40);
	storePublished = true;
	push([{ t: "volume", v: report }]);
	expect(committed.map((d) => d.type)).toEqual([UNMUTE, MUTE, VOLUME]);
});

test("a report that is not a level is skipped without a trace", () => {
	push([{ t: "volume", v: { volume: "loud" } }]);

	expect(committed).toEqual([]);
	expect(reported).toEqual([]);
});
