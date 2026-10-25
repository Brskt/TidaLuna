// Tests for the batch in `src/index.ts` (`__TIDALUNAR_PLAYER_PUSH__`): one event's failure is
// reported and leaves the events behind it alone.
//
// Regression: the loop had no isolation; a consumer that threw on event 1 dropped event 2, which
// could be the `duration` an SDK load waits on with no timeout.

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let triggered: { name: string; target: unknown }[] = [];
let throwOn: string | null = null;
let reducerThrows = false;

const storeStub = () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => {
			if (reducerThrows && a.type === "playbackControls/STOP") throw new Error("reducer exploded");
			dispatched.push(a);
		},
		subscribe: () => () => {},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);

bootStubs();

await import("../src/index");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);

let reported: unknown[][] = [];

beforeEach(() => {
	dispatched = [];
	triggered = [];
	throwOn = null;
	reducerThrows = false;
	reported = [];
	(window as any).NativePlayerComponent = {
		trigger: (name: string, target: unknown) => {
			triggered.push({ name, target });
			if (name === throwOn) throw new Error(`${name} listener exploded`);
		},
	};
	console.error = (...args: unknown[]) => void reported.push(args);
	console.warn = () => {};
});

test("a consumer that throws on event 1 does not keep event 2 from being handled", () => {
	throwOn = "mediaformat";

	expect(() => push([{ t: "mediaformat", v: { codec: "flac" } }, { t: "duration", v: 215, seq: 3 }])).not.toThrow();

	expect(triggered.map((t) => t.name)).toEqual(["mediaformat", "mediaduration"]);
});

test("a refusal that fails is reported and the events behind it still arrive", () => {
	reducerThrows = true;

	push([{ t: "medianetworklost" }, { t: "duration", v: 215, seq: 3 }]);

	expect(triggered.map((t) => t.name)).toEqual(["mediaerror", "mediaduration"]);
	expect(reported.length).toBe(1);
	expect(reported[0]?.[1]).toBe("medianetworklost");
});
