// Tests the registry `src/plugins/loader.ts` fills before `@luna/ui` loads, and its reconciliation
// once the startup pass ran. The tests of this file share one early fill and follow one boot in order.

import { expect, mock, test } from "bun:test";

const row = (url: string, extra: Record<string, unknown> = {}) => {
	const name = url.split("/").pop();
	return { url, name, manifest: JSON.stringify({ name }), enabled: true, installed: true, ...extra };
};

// What Rust holds; each `plugin.list` answers a copy taken when it is asked.
let rows: any[] = [];
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		if (channel === "plugin.list") return structuredClone(rows);
		if (channel === "plugin.enable" || channel === "plugin.disable" || channel === "plugin.update_all") return true;
		return ipc.invokeIpc(channel, ...args);
	},
}));

(window as any).luna ??= {};
const { LunaPlugin: CoreLunaPlugin, modules } = await import("../render/src");
const loader = await import("../src/plugins/loader");
const { LunaPlugin } = await import("../src/plugins/luna-plugin");

const OK = "https://x/Ok";
const FAILED = "https://x/Failed";
const DUPLICATE = "https://x/Duplicate";
const LIB = "https://x/Lib";

// A Store card appears as soon as `@luna/ui` renders. Built from an empty registry, an installed
// plugin would show Install, and a click would reinstall it over the user's (trust and flag reset).
test("a Store card of an installed plugin is the installed plugin", async () => {
	rows = [row(OK), row(FAILED), row(DUPLICATE), row(LIB)];
	await loader.loadRegistry();

	const card = await (CoreLunaPlugin as any).fromStorage({ url: `${LIB}.json` });

	expect(card).toBe(LunaPlugin.plugins[LIB]);
	expect(card.installed).toBe(true);
	expect(LunaPlugin.getByName("Lib")).toBe(card);
});

// The startup pass turns a plugin off and drops a duplicate row with no event. The page must
// follow the rows, but not undo what the user did meanwhile with a row read before it.
test("the late reconciliation follows the pass and keeps what the user did", async () => {
	const failed = LunaPlugin.plugins[FAILED];
	const turnedOff = LunaPlugin.plugins[OK];
	await turnedOff?.disable();
	// Read before the user's disable landed: it still says enabled.
	rows = [row(OK), row(FAILED, { enabled: false }), row(LIB)];
	modules["@luna/core"] = {} as any;
	globalThis.fetch = (() => Promise.reject(new Error("offline"))) as any;

	await loader.exposeLoaderApi();

	// Updated in place: a card already on screen holds this object.
	expect(LunaPlugin.plugins[FAILED]).toBe(failed);
	expect(failed?.enabled).toBe(false);
	expect(LunaPlugin.plugins[DUPLICATE]).toBeUndefined();
	expect((CoreLunaPlugin.plugins as any)[DUPLICATE]).toBeUndefined();
	expect(turnedOff?.enabled).toBe(false);
	expect(LunaPlugin.plugins["@luna/ui"]).toBeDefined();
});
