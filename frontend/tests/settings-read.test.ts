// Tests how `src/plugins/luna-plugin.ts` reads the Settings a running plugin registered.
// `window.__pluginExports` is a global any plugin can write: a read that threw stopped the startup
// loop that lists plugins, the rigging one included, and left nothing in the UI to switch it off.

import { expect, test } from "bun:test";

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

test("a rigged export table reads as no Settings", () => {
	console.error = () => {};
	const plugin = LunaPlugin.fromPluginInfo({
		url: "https://x/victim",
		name: "Victim",
		manifest: "{}",
		enabled: true,
		installed: true,
	});
	(window as any).__pluginExports = new Proxy(
		{},
		{
			get() {
				throw new Error("rigged");
			},
		},
	);
	expect(plugin.extractSettings()).toBe(false);
});
