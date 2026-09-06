import { unloadSet, type LunaUnload, type LunaUnloads } from "@luna/core";

import { safeTimeout } from "../helpers";
import { store } from "./store";
import type { TidalStoreState } from "./types/store";

/**
 * Wait for the STATE an operation produces, rather than for the action that announces it.
 *
 * `interceptActionResp` waits on the announcement, and for anything TIDAL emits from a saga
 * that wait can never end: a saga's `put` reaches the store through the dispatch chain
 * `applyMiddleware` froze at setup, while our interceptors hang off the `dispatch` PROPERTY
 * we reassign afterwards. The two never meet; every such wait spends its whole timeout.
 * Nothing announces itself to us; but everything a saga writes lands in the store, and
 * `subscribe` is notified by the base dispatch that sits under the whole chain.
 *
 * Reading the value out of the state is also what makes this indifferent to where TIDAL
 * renders from: a view moving to another slice does not change where the load it triggers
 * writes. The wait keeps working across a migration that would break a shape adapter.
 *
 * `undefined` means the deadline passed without the value appearing: a plain answer rather
 * than a rejection, because the deadline is an ordinary outcome here and the caller has
 * somewhere else to go. It carries no information about WHY: nothing observable distinguishes
 * a load that failed from one still running.
 *
 * Per-value by construction, callers need no lock between them: each waiter reads its own
 * key and ignores every notification that is not about it.
 */
export const awaitStoreValue = <T>(
	read: (state: TidalStoreState) => T | undefined,
	unloads: LunaUnloads,
	timeoutMs = 5000,
): Promise<T | undefined> => {
	// Read before subscribing: the value may already be there, and a subscription only ever
	// fires on the NEXT write. A caller arriving after the one it was waiting for would
	// otherwise wait out the whole deadline for something already in hand.
	const present = read(store.getState());
	if (present !== undefined) return Promise.resolve(present);

	const { resolve, promise } = Promise.withResolvers<T | undefined>();
	const _unloads = new Set<LunaUnload>();

	// The deadline and the subscription race, and the loser still fires. Guarded here rather
	// than left to `unloadSet`, which clears the set only after awaiting every unload, a
	// window the two can both pass through.
	let settled = false;
	const settle = (value: T | undefined) => {
		if (settled) return;
		settled = true;
		// The same two closures sit in the caller's set as well, for a plugin unloading mid-wait
		// to reach them. The wait is over, and they leave it: `unloadSet` empties only the
		// set it is handed, and the caller's is the plugin's own, drained once at teardown. This
		// runs once per uncached row of a list, which is how the leftovers add up.
		if (unloads !== null) for (const unload of _unloads) unloads.delete(unload);
		unloadSet(_unloads);
		resolve(value);
	};

	const unsubscribe = store.subscribe(() => {
		const value = read(store.getState());
		if (value !== undefined) settle(value);
	});
	const unsubscribed: LunaUnload = () => unsubscribe();
	unsubscribed.source = "awaitStoreValue";
	_unloads.add(unsubscribed);
	safeTimeout(_unloads, () => settle(undefined), timeoutMs);

	if (unloads !== null) {
		for (const unload of _unloads) unloads.add(unload);
	}
	return promise;
};
