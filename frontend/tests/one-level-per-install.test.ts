// Tests that this install plays one level whichever TIDAL account a page loads: `catchUpWithRust`
// (`src/volume.ts`), the early runtime's `__lunaRestoreVolume` (`volume_restore.js`) and Rust's
// rule for a page's level (`OutputLevel`, restated by `FakeRust`).
//
// TIDAL keeps a level per account (`_TIDAL_<uid>volumeSettings`) and restores it once the app has
// booted. Every login, logout and reload is a fresh document: a fresh store, a fresh forwarder
// (a fresh module instance here), and the same Rust. JDe is in `helpers/playback-controls.ts`.
//
// Three things happen on a page load, in an order nothing fixes: TIDAL's native player applies
// the store's level when it is built, JDe reads the saved level and later puts it, and the
// page's catch-up leaves and later lands.

import { beforeEach, expect, mock, test } from "bun:test";
import { fragmentSource, runFragment } from "./helpers/early-runtime";
import { INITIAL, playbackControls, type Saved } from "./helpers/playback-controls";
import { FakeRust } from "./helpers/rust-level";
import type { SentIpc } from "./setup";

type VolumeModule = typeof import("../src/volume");

let account = "";
let savedBy: Record<string, Saved> = {};
let sdkApplies = false;
let page: VolumeModule;
const tidal = playbackControls((volume) => {
	// Z8, under the logged-in account's key, then Hwe when TIDAL's player has a stream
	if (account) savedBy[account] = { mute: controls.muted, volume: controls.volume, volumeUnmute: controls.volumeUnmute };
	if (sdkApplies) page.applyVolume(volume);
});
const { controls, store } = tidal;
const storeStub = () => ({ store });
mock.module("../plugins/lib/src/redux/store", storeStub);

beforeEach(() => {
	mock.module("../plugins/lib/src/redux/store", storeStub);
});

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
let rust = new FakeRust();
// Rust takes each message as it arrives, in the order the page sent them.
const rustTakes = () => rust.applies(sent);

type Slot = (saved: Promise<Saved | null>) => Promise<Saved | null>;

let pages = 0;

// A fresh document for `who`: TIDAL's store at its initial state, a forwarder that has taken in
// nothing, and the early runtime's slot pinned on a global of its own.
async function loadPage(who: string, { sdk = false } = {}): Promise<{ steps: Record<Step, () => void | Promise<void>> }> {
	tidal.reset(INITIAL);
	account = who;
	sdkApplies = sdk;
	sent.length = 0;
	page = await import(`../src/volume?page=${++pages}`);
	const scope: Record<string, unknown> = {};
	runFragment(await fragmentSource("volume_restore"), {
		globalThis: scope,
		invokeIpc: () => Promise.resolve({ outputLevel: rust.report() }),
	});
	const slot = scope.__lunaRestoreVolume as Slot;

	let restoring: Promise<Saved | null> | undefined;
	let reply: unknown;
	const steps: Record<Step, () => void | Promise<void>> = {
		// TIDAL's native player is built and applies the store's level.
		player: () => page.applyVolume(100),
		// JDe reads this account's saved level through the slot, which asks Rust now.
		restoreRead: () => {
			const saved = savedBy[who];
			restoring = slot(Promise.resolve(saved ? { ...saved } : null));
		},
		// JDe puts what differs once its read resolved.
		restorePut: async () => tidal.restore(await restoring),
		// `resyncOutputLevel` leaves: Rust counts the catch-up, and init() installs the follower.
		catchUpSent: () => {
			reply = rust.catchUp();
			page.followCommittedVolume(store);
		},
		catchUpLands: () => page.catchUpWithRust(store, reply),
	};
	return { steps };
}

type Step = "player" | "restoreRead" | "restorePut" | "catchUpSent" | "catchUpLands";
const STEPS: Step[] = ["player", "restoreRead", "restorePut", "catchUpSent", "catchUpLands"];

// Every order of `STEPS`: each restore read before its put, the catch-up sent before it lands.
function orders(): Step[][] {
	const all: Step[][] = [];
	const permute = (left: Step[], acc: Step[]) => {
		if (left.length === 0) {
			const at = (s: Step) => acc.indexOf(s);
			if (at("restoreRead") < at("restorePut") && at("catchUpSent") < at("catchUpLands")) all.push(acc);
			return;
		}
		left.forEach((s, i) => permute([...left.slice(0, i), ...left.slice(i + 1)], [...acc, s]));
	};
	permute(STEPS, []);
	return all;
}

async function load(who: string, order: Step[], { sdk = false } = {}) {
	const { steps } = await loadPage(who, { sdk });
	for (const step of order) {
		await steps[step]();
		rustTakes();
	}
}

const CANONICAL: Step[] = ["catchUpSent", "catchUpLands", "restoreRead", "restorePut", "player"];
const slider = (volume: number) => {
	store.dispatch({ type: "playbackControls/SET_VOLUME", payload: { volume } });
	rustTakes();
};

test("every order of a page load is covered", () => {
	expect(orders().length).toBe(30);
});

test("a page of another account, loading after a page set the level, takes that level", async () => {
	for (const sdk of [false, true]) {
		for (const order of orders()) {
			rust = new FakeRust();
			savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 }, bob: { mute: false, volume: 80, volumeUnmute: 80 } };
			await load("alice", CANONICAL);
			slider(30);
			await load("bob", order, { sdk });
			const where = { sdk, order: order.join(",") };
			expect({ ...where, rust: rust.volume, store: controls.volume }).toEqual({ ...where, rust: 30, store: 30 });
			// Bob's own level from here on reaches Rust.
			slider(45);
			expect({ ...where, rust: rust.volume }).toEqual({ ...where, rust: 45 });
		}
	}
});

test("a page of another account, loading after a page muted, stays muted with the level to restore", async () => {
	for (const order of orders()) {
		rust = new FakeRust();
		savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 }, bob: { mute: false, volume: 80, volumeUnmute: 80 } };
		await load("alice", CANONICAL);
		store.dispatch({ type: "playbackControls/TOGGLE_MUTE" });
		rustTakes();
		await load("bob", order, { sdk: true });
		const where = order.join(",");
		expect({ where, rust: rust.report(), controls }).toEqual({
			where,
			rust: { ...rust.report(), volume: 0, unmuteVolume: 70 },
			controls: { volume: 0, volumeUnmute: 70, muted: true },
		});
	}
});

test("a reload of the same account keeps its level", async () => {
	for (const order of orders()) {
		rust = new FakeRust();
		savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 } };
		await load("alice", CANONICAL);
		slider(30);
		await load("alice", order, { sdk: true });
		expect({ order: order.join(","), rust: rust.volume, store: controls.volume }).toEqual({ order: order.join(","), rust: 30, store: 30 });
	}
});

test("on a first launch, TIDAL's saved level seeds the level, whatever the order", async () => {
	for (const sdk of [false, true]) {
		for (const order of orders()) {
			rust = new FakeRust();
			savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 }, bob: { mute: false, volume: 80, volumeUnmute: 80 } };
			await load("alice", order, { sdk });
			const where = { sdk, order: order.join(",") };
			expect({ ...where, rust: rust.volume, chosen: rust.chosen, store: controls.volume }).toEqual({
				...where,
				rust: 70,
				chosen: true,
				store: 70,
			});
			// Bob, next in the same launch, takes it.
			await load("bob", CANONICAL, { sdk });
			expect({ ...where, rust: rust.volume, store: controls.volume }).toEqual({ ...where, rust: 70, store: 70 });
		}
	}
});

test("a first launch that starts logged out still takes the saved level of the account that logs in", async () => {
	rust = new FakeRust();
	savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 } };
	await load("", CANONICAL);
	expect(rust.chosen).toBe(false);
	await load("alice", CANONICAL, { sdk: true });
	expect({ rust: rust.volume, store: controls.volume }).toEqual({ rust: 70, store: 70 });
});

test("a level set by an account with nothing saved is the one the next account takes", async () => {
	rust = new FakeRust();
	savedBy = { bob: { mute: false, volume: 80, volumeUnmute: 80 } };
	await load("alice", CANONICAL);
	slider(30);
	await load("bob", CANONICAL, { sdk: true });
	expect({ rust: rust.volume, store: controls.volume }).toEqual({ rust: 30, store: 30 });
});

test("a plugin's level on a first launch is the level the page then takes", async () => {
	rust = new FakeRust();
	savedBy = { alice: { mute: false, volume: 70, volumeUnmute: 70 } };
	const { steps } = await loadPage("alice", { sdk: true });
	await steps.catchUpSent();
	await steps.catchUpLands();
	rustTakes();
	// A plugin states no count (`sendIpc("player.volume", v)`).
	rust.page(40);
	await steps.restoreRead();
	await steps.restorePut();
	rustTakes();
	// TIDAL restored nothing over it.
	expect({ rust: rust.volume, saved: savedBy.alice?.volume }).toEqual({ rust: 40, saved: 70 });
	// The next page mirrors it.
	await load("alice", CANONICAL, { sdk: true });
	expect({ rust: rust.volume, store: controls.volume }).toEqual({ rust: 40, store: 40 });
});
