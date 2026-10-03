// Tests a page loading after a Connect controller or the OS mixer moved the level Rust plays
// while no page was there to mirror it (the login screen, a reload): `catchUpWithRust`
// (`src/volume.ts`) and the early runtime's `__lunaRestoreVolume` (`volume_restore.js`).
//
// TIDAL keeps a volume memory of its own and restores it once the app has booted. The fake store
// runs TIDAL's `playbackControls` rules (`helpers/playback-controls.ts`), that restore (JDe)
// included, with `readSaved` wrapped the way `route_volume_restore` (module_capture.rs) rewrites it:
//   call(() => (globalThis.__lunaRestoreVolume || (s => s))(readSaved())).
// TIDAL's native player applies a volume the moment it is built (`this.#n.setVolume(100)`,
// nativePlayer chunk), which our delegate's `setVolume` hands to `applyVolume`; a cold start
// restoring a paused track builds one at boot.

import { beforeEach, expect, mock, test } from "bun:test";
import { fragmentSource, runFragment } from "./helpers/early-runtime";
import { INITIAL, playbackControls, type Saved } from "./helpers/playback-controls";
import { FakeRust, rustReport } from "./helpers/rust-level";
import type { SentIpc } from "./setup";

let saved: Saved | null = null;
let sdkApplies = false;
// False until our code has found TIDAL's store: the lib's proxy throws on any access before.
let storeFound = true;

const tidal = playbackControls((volume) => {
	// Z8, then Hwe when TIDAL's active player has a stream to apply it to
	saved = { mute: controls.muted, volume: controls.volume, volumeUnmute: controls.volumeUnmute };
	if (sdkApplies) applyVolume(volume);
});
const { controls } = tidal;

// The store as our code reaches it (plugins/lib's proxy).
const store = {
	dispatch(action: { type: string; payload?: any }) {
		if (!storeFound) throw new Error("[luna:lib] Redux store not initialized yet!");
		tidal.store.dispatch(action);
	},
	getState() {
		if (!storeFound) throw new Error("[luna:lib] Redux store not initialized yet!");
		return { playbackControls: controls };
	},
};
const storeStub = () => ({ store });
mock.module("../plugins/lib/src/redux/store", storeStub);

const { applyVolume, adoptRustVolume, catchUpWithRust, mirrorRustLevel } = await import("../src/volume");

// Rust as the page sees it: the level it plays, and its rule for a page's level.
let rust = new FakeRust();
const rustRemote = (volume: number, unmuteVolume: number) => rust.remote(volume, unmuteVolume);
const rustPage = (volume: number) => rust.page(volume);

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const volumeWrites = () => sent.filter((s) => s.channel === "player.volume").map((s) => s.args);
const rustApplies = () => rust.applies(sent);
// The page's catch-up (`resyncOutputLevel`): Rust counts it, and the page takes in its report.
const catchUp = () => catchUpWithRust(store, rust.catchUp());

type Slot = (saved: Promise<Saved | null>) => Promise<Saved | null>;

// The early runtime's slot, on a global of its own per test since it pins itself.
async function restoreSlot(getState: () => Promise<unknown>): Promise<Slot> {
	const scope: Record<string, unknown> = {};
	runFragment(await fragmentSource("volume_restore"), { globalThis: scope, invokeIpc: getState });
	return scope.__lunaRestoreVolume as Slot;
}
const rustState = () => Promise.resolve({ outputLevel: rust.report() });

// TIDAL's JDe through the rewritten call. It reads the saved level when it starts and puts the
// differences once the read resolves, which can be after our code caught up.
function tidalRestore(slot: Slot | undefined): Promise<void> {
	const read = Promise.resolve(saved && { ...saved });
	return (async () => {
		const e = await (slot ?? ((s: Promise<Saved | null>) => s))(read);
		tidal.restore(e);
	})();
}

// A fresh document: TIDAL's store starts from its initial state, and the saved level is what the
// last page persisted. The page's forwarder is module state, as Rust's level is: a fresh one holds
// no level, which -1 stands for since no store holds it.
function newPage(savedLevel: Saved, { sdk = false } = {}) {
	tidal.reset(INITIAL);
	saved = { ...savedLevel };
	sdkApplies = sdk;
	storeFound = true;
	adoptRustVolume(-1);
	sent.length = 0;
}

beforeEach(() => {
	mock.module("../plugins/lib/src/redux/store", storeStub);
});

const SAVED_70: Saved = { mute: false, volume: 70, volumeUnmute: 70 };
const ORDERS = ["restore, then catch-up", "restore read, catch-up, restore put", "catch-up, then restore"];

test("a page loading after a remote mute shows it muted, whenever TIDAL's restore runs", async () => {
	for (const order of ORDERS) {
		rustRemote(0, 70);
		newPage(SAVED_70);
		const slot = await restoreSlot(rustState);
		if (order === ORDERS[0]) {
			await tidalRestore(slot);
			catchUp();
		} else if (order === ORDERS[1]) {
			const restoring = tidalRestore(slot);
			catchUp();
			await restoring;
		} else {
			catchUp();
			await tidalRestore(slot);
		}
		expect({ order, controls }).toEqual({ order, controls: { volume: 0, volumeUnmute: 70, muted: true } });
		// What TIDAL restores on the next load, should Rust's level not be the newer one by then.
		expect({ order, saved }).toEqual({ order, saved: { mute: true, volume: 0, volumeUnmute: 70 } });
	}
});

test("a page loading after a remote level shows that level, not the saved one", async () => {
	rustRemote(30, 30);
	newPage(SAVED_70);
	const restoring = tidalRestore(await restoreSlot(rustState));
	catchUp();
	await restoring;
	expect(controls).toEqual({ volume: 30, volumeUnmute: 30, muted: false });
});

test("a player built before the page caught up cannot undo a remote mute", async () => {
	// The cold restore of a paused track: TIDAL has put its saved 70 in the store and builds its
	// native player, whose first volume leaves before the page has mirrored Rust's level.
	const muted = rustRemote(0, 70);
	newPage(SAVED_70);
	await tidalRestore(undefined);
	applyVolume(100);
	const [[volume, seen]] = volumeWrites();
	expect(volume).toBe(70);
	expect(seen as number).toBeLessThan(muted.seq);
	rustApplies();
	expect(rust.seq).toBe(muted.seq);
});

test("TIDAL applying the caught-up level sends nothing back", async () => {
	rustRemote(0, 55);
	newPage(SAVED_70, { sdk: true });
	await tidalRestore(await restoreSlot(rustState));
	catchUp();
	applyVolume(100);
	expect(volumeWrites()).toEqual([]);
});

test("after the page caught up, the user's level reaches Rust", async () => {
	rustRemote(0, 55);
	newPage(SAVED_70, { sdk: true });
	await tidalRestore(await restoreSlot(rustState));
	catchUp();
	store.dispatch({ type: "playbackControls/TOGGLE_MUTE" });
	expect(controls.volume).toBe(55);
	rustApplies();
	expect(rust.volume).toBe(55);
});

test("while nobody chose a level, TIDAL's saved level is restored and reaches Rust", async () => {
	// A first launch: Rust sits at its start level, which is nobody's choice.
	rust = new FakeRust();
	newPage(SAVED_70);
	await tidalRestore(await restoreSlot(rustState));
	catchUp();
	expect(controls).toEqual({ volume: 70, volumeUnmute: 70, muted: false });
	applyVolume(100);
	rustApplies();
	expect(rust.volume).toBe(70);
});

test("after a reload following the page's own last level, the user's next level reaches Rust", async () => {
	// A controller moved the level once, then the page set its own: that one is TIDAL's saved
	// level and stands, and the page must know Rust is past the controller's move.
	rustRemote(30, 30);
	rustPage(45);
	newPage({ mute: false, volume: 45, volumeUnmute: 45 }, { sdk: true });
	await tidalRestore(await restoreSlot(rustState));
	catchUp();
	expect(controls.volume).toBe(45);
	store.dispatch({ type: "playbackControls/SET_VOLUME", payload: { volume: 60 } });
	rustApplies();
	expect(rust.volume).toBe(60);
});

test("a remote move the store could not take yet is caught up with once it is found", async () => {
	newPage(SAVED_70);
	await tidalRestore(undefined);
	// The relay mirrors into a store our code has not found yet: the dispatch throws, and the
	// event bus logs and drops the error (ipc.ts).
	storeFound = false;
	expect(() => mirrorRustLevel(store, rustRemote(0, 70))).toThrow();
	storeFound = true;
	// Until then the page's levels are ones decided without knowing of that mute.
	applyVolume(100);
	rustApplies();
	expect(rust.volume).toBe(0);
	catchUp();
	expect(controls).toEqual({ volume: 0, volumeUnmute: 70, muted: true });
});

test("a report older than one the page mirrored changes nothing", () => {
	newPage(SAVED_70);
	const older = rustReport(20, 20);
	const newer = rustReport(40, 40);
	catchUpWithRust(store, newer);
	catchUpWithRust(store, older);
	expect(controls.volume).toBe(40);
});

test("TIDAL's restore reads its saved level untouched when Rust cannot answer", async () => {
	rustRemote(0, 70);
	newPage(SAVED_70);
	await tidalRestore(await restoreSlot(() => Promise.reject(new Error("Connect not ready"))));
	expect(controls).toEqual({ volume: 70, volumeUnmute: 70, muted: false });
});

test("the slot is pinned: a later script cannot swap it", async () => {
	const scope: Record<string, unknown> = {};
	runFragment(await fragmentSource("volume_restore"), { globalThis: scope, invokeIpc: rustState });
	expect(() => Object.defineProperty(scope, "__lunaRestoreVolume", { value: () => null })).toThrow();
});
