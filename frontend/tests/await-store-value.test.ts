// Tests `plugins/lib/src/redux/awaitStoreValue.ts`, the wait that replaced waiting on a
// Redux action for TIDAL's own single-item load.
//
// Why it exists at all: TIDAL answers that load from a saga, and a saga's `put` reaches the
// store through the dispatch chain `applyMiddleware` froze at setup, never through the
// `dispatch` property our interceptors are hung off. Waiting on the SUCCESS action therefore
// spent the whole deadline on every call, holding a lock shared with every other lookup; a
// list of misses resolved one item per deadline. `subscribe` sits under the whole chain and
// sees what a saga writes, which is why the wait now reads the state instead.
//
// The property each test pins is what a mutation would break: delete the subscription and the
// first test hangs to its deadline; delete the pre-read and the second does.

import { beforeEach, expect, mock, test } from "bun:test";

type Listener = () => void;

let state: { content: { mediaItems: Record<string, unknown> } };
let listeners: Listener[];
let unsubscribes: number;

const storeStub = () => ({
	store: {
		getState: () => state,
		subscribe: (listener: Listener) => {
			listeners.push(listener);
			return () => {
				unsubscribes++;
				const at = listeners.indexOf(listener);
				if (at !== -1) listeners.splice(at, 1);
			};
		},
	},
});
mock.module("../plugins/lib/src/redux/store", storeStub);
// The real one traces failures through the core; here only its contract matters: call every
// unload synchronously, then empty the set once they have all been awaited.
mock.module("@luna/core", () => ({
	unloadSet: async (unloads?: Set<() => void>) => {
		if (unloads === undefined) return;
		await Promise.all([...unloads].map(async (unload) => unload()));
		unloads.clear();
	},
}));
// The helpers barrel reaches the native dash and credential modules, none of which this
// wait touches. Stubbed to the one helper it does use, kept faithful because the deadline
// under test is a real timer.
mock.module("../plugins/lib/src/helpers", () => ({
	safeTimeout: (unloads: Set<() => void> | null, cb: () => void, delay?: number) => {
		const timeout = setTimeout(cb, delay);
		const unload = () => clearTimeout(timeout);
		unloads?.add(unload);
		return unload;
	},
}));

const { awaitStoreValue } = await import("../plugins/lib/src/redux/awaitStoreValue");

/** What a saga's reducer does: write, then notify - the only signal we ever get. */
const write = (id: string, value: unknown) => {
	state.content.mediaItems[id] = value;
	for (const listener of [...listeners]) listener();
};

beforeEach(() => {
	state = { content: { mediaItems: {} } };
	listeners = [];
	unsubscribes = 0;
});

test("a value written by someone we cannot hear still ends the wait", () => {
	// The defect this replaced: nothing dispatches an action we can intercept, and the old
	// wait had no other way to learn the load had landed.
	const waiting = awaitStoreValue((s: any) => s.content.mediaItems["1"], null, 50);
	write("1", { type: "track", item: { id: 1 } });

	return waiting.then((value) => {
		expect(value).toEqual({ type: "track", item: { id: 1 } });
		expect(listeners).toHaveLength(0);
	});
});

test("a value already in the store is not waited for", async () => {
	// A subscription only fires on the NEXT write, and a caller arriving after the write it
	// was waiting for would otherwise sit out the whole deadline for something in hand.
	state.content.mediaItems["2"] = { type: "track", item: { id: 2 } };

	expect(await awaitStoreValue((s: any) => s.content.mediaItems["2"], null, 50)).toEqual({ type: "track", item: { id: 2 } });
	expect(listeners).toHaveLength(0);
});

test("a deadline with nothing written answers undefined rather than throwing", async () => {
	// The caller has somewhere else to go (a direct fetch), and a region-blocked track
	// reaches this on every call: an ordinary outcome, not an error.
	expect(await awaitStoreValue((s: any) => s.content.mediaItems["3"], null, 20)).toBeUndefined();
	expect(listeners).toHaveLength(0);
});

// The set a caller hands in is the plugin's own and lives as long as the plugin does. The wait
// puts its teardown there for a plugin unloading mid-wait to reach it; a wait that has ended
// has nothing left to tear down, and this path runs once per uncached row of a list.
test("a wait that has ended leaves nothing in the caller's set", async () => {
	const unloads = new Set<any>();
	const waiting = awaitStoreValue((s: any) => s.content.mediaItems["7"], unloads, 50);
	expect(unloads.size).toBeGreaterThan(0);

	write("7", { type: "track", item: { id: 7 } });
	await waiting;

	expect(unloads.size).toBe(0);
});

// The deadline is the other way in to the same settle, and the way a region-blocked track always
// leaves: it must clean up the same.
test("a wait that ran out of time leaves nothing in the caller's set", async () => {
	const unloads = new Set<any>();

	expect(await awaitStoreValue((s: any) => s.content.mediaItems["8"], unloads, 20)).toBeUndefined();

	expect(unloads.size).toBe(0);
});

test("two waits do not block each other, whatever order they land in", async () => {
	// The lock this removed was global to the action type: one lookup's deadline was every
	// other lookup's deadline. Each wait reads its own key and ignores the rest.
	const first = awaitStoreValue((s: any) => s.content.mediaItems["5"], null, 100);
	const second = awaitStoreValue((s: any) => s.content.mediaItems["6"], null, 100);

	write("6", { type: "track", item: { id: 6 } });
	expect(await second).toEqual({ type: "track", item: { id: 6 } });

	write("5", { type: "track", item: { id: 5 } });
	expect(await first).toEqual({ type: "track", item: { id: 5 } });
});

// Redux notifies a snapshot of its listeners, and an unsubscribe during a dispatch leaves a
// listener already in it. A subscriber ahead of ours that dispatches from inside its listener
// reaches ours twice for one write, in the nested dispatch and then in the outer one. Only the
// first may tear down; the set still holds the unloads until they have all been awaited.
test("a notification delivered twice for one write tears the wait down once", async () => {
	let nested = false;
	listeners.push(() => {
		if (nested) return;
		nested = true;
		for (const listener of [...listeners]) listener();
	});
	const waiting = awaitStoreValue((s: any) => s.content.mediaItems["9"], null, 50);

	write("9", { type: "track", item: { id: 9 } });

	expect(await waiting).toEqual({ type: "track", item: { id: 9 } });
	expect(unsubscribes).toBe(1);
});
