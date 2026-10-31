// Tests for `plugins/ui/src/wireSnapshot.ts`, over a plain store.
// Mocked: `@luna/lib`, whose `on` hands its unload to `unloads` like `ipc.ts` and whose `invoke` answers when told.

import { expect, mock, test } from "bun:test";
import type { LunaUnloads } from "@luna/core";
import { createStore } from "../plugins/ui/src/store";

const CHANNEL = "settings.audio_cache_state";
const handlers = new Map<string, (payload: unknown) => void>();
const pulls: ((value: unknown) => void)[] = [];
let ons = 0;

mock.module("@luna/lib", () => ({
	ipcRenderer: {
		on(unloads: LunaUnloads, channel: string, listener: (payload: unknown) => void) {
			ons++;
			handlers.set(channel, listener);
			const unload = () => handlers.delete(channel);
			unloads.add(unload);
			return unload;
		},
		invoke() {
			return new Promise((resolve) => pulls.push(resolve));
		},
		send() {},
	},
}));

const { wireSnapshot } = await import("../plugins/ui/src/wireSnapshot");

function newUnloads() {
	const fns: (() => void)[] = [];
	return { unloads: { add: (fn: () => void) => void fns.push(fn) } as unknown as LunaUnloads, run: () => fns.forEach((fn) => fn()) };
}

// A macrotask turn comes only after the microtask queue empties, every `.then`/`.catch` hop of a settled
// pull having run, whatever their number. Not a timer: nothing waits for a duration.
const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

test("a push that lands during the pull wins over its older answer, and the unloads let the store wire again", async () => {
	const store = createStore(0);
	const wire = wireSnapshot(store, CHANNEL, (held: number, raw) => Math.max(held, raw as number));
	const first = newUnloads();
	wire(first.unloads);
	expect(pulls).toHaveLength(1);
	handlers.get(CHANNEL)!(2);
	pulls[0]!(1);
	await flush();
	expect(store.get()).toBe(2);

	first.run();
	expect(handlers.has(CHANNEL)).toBe(false);
	wire(newUnloads().unloads);
	expect(ons).toBe(2);
	expect(pulls).toHaveLength(2);
	expect(handlers.has(CHANNEL)).toBe(true);
});
