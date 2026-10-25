// Tests what `src/plugins/luna-plugin.ts` leaves on a plugin's card when the plugin cannot load.
// The card reads `loadError`: a reason that stops short of it leaves the plugin switched off with
// nothing said, which is what the startup pass and a refused enable both did.

import { expect, mock, test } from "bun:test";

const refusal = 'plugin name "." is not allowed: each part must start with a letter or a digit';
const REFUSED_URL = "https://x/enable-refused";

// Rust's answer to one enable, the one url below. Module mocks hold for the whole process: every
// other call goes to the real `invokeIpc` the preload wires up.
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: (channel: string, ...args: any[]) =>
		channel === "plugin.enable" && args[0] === REFUSED_URL
			? Promise.reject(new Error(`Cannot enable: ${refusal}`))
			: ipc.invokeIpc(channel, ...args),
}));

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

test("a listed plugin that cannot load shows why", () => {
	const plugin = LunaPlugin.fromPluginInfo({
		url: "https://x/listed-refused",
		name: ".",
		manifest: "{}",
		enabled: false,
		installed: true,
		loadError: refusal,
	});
	expect(plugin.loadError._).toBe(refusal);
});

test("an enable Rust refuses leaves its reason on the card", async () => {
	console.error = () => {};
	const plugin = LunaPlugin.fromPluginInfo({
		url: REFUSED_URL,
		name: "Refused",
		manifest: "{}",
		enabled: false,
		installed: true,
	});
	await plugin.enable();
	expect(plugin.enabled).toBe(false);
	expect(plugin.loadError._).toContain(refusal);
});
