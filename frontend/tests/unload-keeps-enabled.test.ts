// A page unload leaves every plugin enabled. Our instances share the render core's registry, and
// nothing run at unload may reach `plugin.disable`: it writes enabled = 0, and every plugin would be
// off at the next launch.

import { expect, test } from "bun:test";

(window as any).luna = (window as any).luna ?? {};

const { LunaPlugin: CoreLunaPlugin } = await import("../render/src");
await import("../src/plugins/loader");
const { LunaPlugin } = await import("../src/plugins/luna-plugin");

test("a page unload sends no plugin.disable for a registered plugin", async () => {
	const plugin = LunaPlugin.fromPluginInfo({ url: "https://x/unload-keeps-enabled", name: "Kept", manifest: "{}", enabled: true, installed: true });
	(CoreLunaPlugin.plugins as any)[plugin.url] = plugin;
	const sent = (globalThis as any).__LUNAR_TEST_IPC__ as { channel: string; args: unknown[] }[];
	const before = sent.length;

	window.dispatchEvent(new Event("beforeunload"));
	await new Promise((resolve) => setTimeout(resolve, 0));

	expect(sent.slice(before).filter((s) => s.channel === "plugin.disable")).toEqual([]);
});
