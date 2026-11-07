// Tests how `src/plugins/loader.ts` takes a store move from Rust. Rust emits `plugin_moved`, then at
// once what befalls the plugin at its new url, while the page takes the move only once a row read
// confirms it. The tests of this file follow one page in order.

import { expect, mock, test } from "bun:test";

const FROM = "https://old/Mover";
const TO = "https://new/Mover";
const OTHER = "https://x/Other";
const VICTIM = "https://x/Victim";
const LATE = "https://late/Mover";
const FRESH = "https://x/Fresh";

const row = (url: string) => {
	const name = url.split("/").pop();
	return { url, name, manifest: JSON.stringify({ name }), enabled: true, installed: true };
};

// What Rust holds, read when a `plugin.list` is answered; `held` keeps the answers back.
let rows: any[] = [];
let listFails = false;
let held: Promise<void> = Promise.resolve();
let open = () => {};
const hold = () => {
	held = new Promise((resolve) => (open = resolve));
};
let fetched: Promise<void> = Promise.resolve();
let fetchOpen = () => {};

const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		if (channel === "plugin.list") {
			await held;
			if (listFails) throw new Error("database busy");
			return structuredClone(rows);
		}
		if (channel === "plugin.fetch_package") {
			await fetched;
			return { name: "Mover" };
		}
		return ipc.invokeIpc(channel, ...args);
	},
}));

(window as any).luna ??= {};
await import("../render/src");
const loader = await import("../src/plugins/loader");
const { LunaPlugin } = await import("../src/plugins/luna-plugin");

const emit = (channel: string, ...args: unknown[]) => (window as any).__LUNAR_IPC_EMIT__(channel, ...args);
const answered = () => new Promise((resolve) => setTimeout(resolve, 0));

// A plugin first loaded at its new address can fail at once: that event came before the page took
// the move and was dropped, leaving the card on with no error while Rust had turned it off.
test("an event for the new url waits for the move and reaches the moved plugin", async () => {
	rows = [row(FROM), row(OTHER), row(VICTIM)];
	await loader.loadRegistry();
	const mover = LunaPlugin.plugins[FROM]!;
	// The Store tab, open behind the move dialog, made a card for the new address.
	const placeholder = await LunaPlugin.fromStorage({ url: TO, package: { name: "Mover" } } as any);

	rows = [row(TO), row(OTHER), row(VICTIM)];
	hold();
	emit("jsrt.plugin_moved", FROM, TO);
	emit("jsrt.plugin_failed", TO, "Missing dependency");
	expect(placeholder.loadError._).toBeUndefined();
	open();
	await answered();

	expect(LunaPlugin.plugins[TO]).toBe(mover);
	expect(LunaPlugin.plugins[FROM]).toBeUndefined();
	expect(mover.enabled).toBe(false);
	expect(mover.loadError._).toBe("Missing dependency");
});

// Any page script can call the emitter. Each forgery below meets another rule: a new url the rows
// do not hold, an old url they still hold, a target already installed, a non-string the registry
// lookup would coerce, a Store placeholder. The events the forgeries held still arrive.
test("a forged move moves nothing and delays the events it held only", async () => {
	const other = LunaPlugin.plugins[OTHER]!;
	const victim = LunaPlugin.plugins[VICTIM]!;
	const mover = LunaPlugin.plugins[TO]!;
	const placeholder = await LunaPlugin.fromStorage({ url: "https://x/Placeholder", package: { name: "P" } } as any);

	// Victim's row is gone, and Fresh is a row no card shows.
	rows = [row(TO), row(OTHER), row(FRESH)];
	hold();
	emit("jsrt.plugin_moved", OTHER, VICTIM);
	emit("jsrt.plugin_moved", OTHER, FRESH);
	emit("jsrt.plugin_moved", VICTIM, TO);
	emit("jsrt.plugin_moved", [OTHER], FRESH);
	emit("jsrt.plugin_moved", "https://x/Placeholder", FRESH);
	emit("jsrt.plugin_stopped", VICTIM, "Stopped by its update");
	open();
	await answered();

	expect(LunaPlugin.plugins[OTHER]).toBe(other);
	expect(other.url).toBe(OTHER);
	expect(LunaPlugin.plugins[VICTIM]).toBe(victim);
	expect(victim.url).toBe(VICTIM);
	expect(LunaPlugin.plugins[TO]).toBe(mover);
	expect(LunaPlugin.plugins[FRESH]).toBeUndefined();
	expect(placeholder.url).toBe("https://x/Placeholder");
	expect(victim.loadError._).toBe("Stopped by its update");
});

// A move whose rows cannot be read must not hold the plugin's events for good.
test("a move the rows could not confirm lets its events through", async () => {
	const victim = LunaPlugin.plugins[VICTIM]!;
	console.warn = () => {};
	listFails = true;
	hold();
	emit("jsrt.plugin_moved", OTHER, VICTIM);
	emit("jsrt.plugin_failed", VICTIM, "held");
	open();
	await answered();
	listFails = false;

	expect(victim.loadError._).toBe("held");
	emit("jsrt.plugin_failed", VICTIM, "after");
	expect(victim.loadError._).toBe("after");
});

// A Store card whose package was still loading when the move landed put its placeholder over the
// moved plugin, and every later event for that url reached the placeholder.
test("a Store card made during a move gets the moved plugin", async () => {
	const mover = LunaPlugin.plugins[TO]!;
	fetched = new Promise((resolve) => (fetchOpen = resolve));
	const card = LunaPlugin.fromStorage({ url: LATE });

	rows = [row(LATE), row(OTHER), row(VICTIM)];
	emit("jsrt.plugin_moved", TO, LATE);
	await answered();
	fetchOpen();

	expect(await card).toBe(mover);
	expect(LunaPlugin.plugins[LATE]).toBe(mover);
});
