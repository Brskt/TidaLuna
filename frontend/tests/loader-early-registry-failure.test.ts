// Tests the early registry fill of `src/plugins/loader.ts` when Rust cannot list the plugins. It
// runs before `@luna/ui` and `jsrt.load_plugins` in `init`: a throw out of it loads no plugin.

import { expect, mock, test } from "bun:test";

const URL_ = "https://x/Orphan";
let listing: "fail" | "ok" = "fail";
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		if (channel === "plugin.list") {
			if (listing === "fail") throw new Error("database busy");
			return [{ url: URL_, name: "Orphan", manifest: '{"name":"Orphan"}', enabled: true, installed: true }];
		}
		if (channel === "plugin.update_all") return true;
		return ipc.invokeIpc(channel, ...args);
	},
}));

(window as any).luna ??= {};
const { LunaPlugin: CoreLunaPlugin, modules } = await import("../render/src");
const loader = await import("../src/plugins/loader");
const { LunaPlugin } = await import("../src/plugins/luna-plugin");

test("a failed list lets the boot go on, and the late reconciliation adopts the cards made meanwhile", async () => {
	console.warn = () => {};
	await loader.loadRegistry();
	const card = await LunaPlugin.fromStorage({ url: URL_, package: { name: "Orphan" } } as any);
	expect(card.installed).toBe(false);

	listing = "ok";
	modules["@luna/core"] = {} as any;
	globalThis.fetch = (() => Promise.reject(new Error("offline"))) as any;
	await loader.exposeLoaderApi();

	expect(card.installed && card.enabled).toBe(true);
	expect((CoreLunaPlugin.plugins as any)[URL_]).toBe(card);
});
