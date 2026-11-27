// Tests how the bridge in `src/index.ts` answers Rust's `medianetworklost`. A loss after the SDK's
// load resolved reaches it: `mediamaxconnectionsreached` then `idle`, which it turns into NPN01
// and TIDAL's banner. Any other has no load to end it and keeps the refusal (STOP, IDLE).

import { beforeEach, expect, mock, test } from "bun:test";
import { bootStubs } from "./helpers/boot-stubs";

type Dispatch = { type: string; payload?: unknown };

let dispatched: Dispatch[] = [];
let triggered: string[] = [];
// The SDK's `#handleNetworkError`: armed by `mediamaxconnectionsreached`, it raises NPN01 on
// the next `idle` and only on one that comes after it.
let waitingForIdle = false;
let networkErrors = 0;

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {} }),
		dispatch: (a: Dispatch) => void dispatched.push(a),
		subscribe: () => () => {},
	},
}));

bootStubs();

await import("../src/index");
const { setSelfLoad } = await import("../src/audio-proxy");

const push = (events: unknown[]) => (window as any).__TIDALUNAR_PLAYER_PUSH__(events);

beforeEach(() => {
	dispatched = [];
	triggered = [];
	waitingForIdle = false;
	networkErrors = 0;
	setSelfLoad(false);
	(window as any).NativePlayerComponent = {
		trigger: (name: string, target: unknown) => {
			triggered.push(name === "mediastate" ? `mediastate:${target}` : name);
			if (name === "mediamaxconnectionsreached") waitingForIdle = true;
			else if (name === "mediastate" && target === "idle" && waitingForIdle) {
				waitingForIdle = false;
				networkErrors++;
			}
		},
	};
});

const loss = (v: unknown, cast = false) => [
	{ t: "medianetworklost", v, cast },
	...(v && (v as { announced?: boolean }).announced ? [{ t: "state", v: "idle", seq: 4, cast }] : []),
	{ t: "state", v: "stopped", seq: 4, cast },
];
const refused = () => dispatched.some((a) => a.type === "playbackControls/STOP");

test("a loss after the load resolved reaches the SDK as its own, in the order it waits on", () => {
	push(loss({ announced: true }));

	expect(triggered).toEqual(["mediamaxconnectionsreached", "mediastate:idle", "mediastate:stopped"]);
	expect(networkErrors).toBe(1);
	expect(refused()).toBe(false);
});

test("a loss before the length was announced, of a self-loaded track or of a cast keeps the refusal", () => {
	const others: [string, boolean, boolean, boolean][] = [
		// [case, announced, self-loaded, cast]
		["the load still waits", false, false, false],
		["the SDK holds no session", true, true, false],
		["a cast's track is no load of the SDK's", true, false, true],
	];
	for (const [name, announced, selfLoad, cast] of others) {
		dispatched = [];
		triggered = [];
		setSelfLoad(selfLoad);
		push(loss({ announced }, cast));

		expect(triggered, name).not.toContain("mediamaxconnectionsreached");
		expect(refused(), name).toBe(true);
	}
});
