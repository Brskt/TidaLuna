// Tests what `src/plugins/luna-plugin.ts` leaves on a plugin's card when the plugin cannot load.
// The card reads `loadError`: a reason that stops short of it leaves the plugin switched off with
// nothing said, which is what the startup pass and a refused enable both did.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";

const refusal = 'plugin name "." is not allowed: each part must start with a letter or a digit';
const REFUSED_URL = "https://x/enable-refused";
const KEPT_URL = "https://x/enable-kept";
const OFF_URL = "https://x/enable-off";
const RACED_URL = "https://x/enable-raced";
const DEP_URL = "https://x/dep-stopped";
const DEPENDANT_URL = "https://x/dependant";
// What Rust answers when this machine, not the plugin, failed: the row keeps its flag.
const undelivered = "The page did not take the plugin in time";

// Rust's answers for this file's urls. Module mocks hold for the whole process: every other call
// goes to the real `invokeIpc` the preload wires up, and `plugin.list` is answered here only while a
// test of this file runs. The log other files wait on never gets one from here.
const refusals = new Map<string, string>([
	[REFUSED_URL, `Cannot enable: ${refusal}`],
	[KEPT_URL, undelivered],
	[OFF_URL, undelivered],
	[RACED_URL, undelivered],
	[DEP_URL, undelivered],
]);
let listed: { rows: any[]; gate?: Promise<void> } | undefined;
const enableCalls: string[] = [];
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: (channel: string, ...args: any[]) => {
		const list = listed;
		if (channel === "plugin.list" && list) {
			return (async () => {
				await list.gate;
				return list.rows;
			})();
		}
		if (channel === "plugin.enable" && refusals.has(args[0])) {
			enableCalls.push(args[0]);
			return Promise.reject(new Error(refusals.get(args[0])));
		}
		if (channel === "plugin.disable" && args[0] === RACED_URL) return Promise.resolve(true);
		if (channel === "plugin.enable" && args[0] === DEPENDANT_URL) {
			enableCalls.push(args[0]);
			return Promise.resolve(true);
		}
		return ipc.invokeIpc(channel, ...args);
	},
}));

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

beforeEach(() => {
	listed = { rows: [] };
	enableCalls.length = 0;
});
afterEach(() => {
	listed = undefined;
});

// The row read is not awaited by `enable()`: let it land.
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

const listedPlugin = (url: string, name: string, manifest: object = {}) =>
	LunaPlugin.fromPluginInfo({ url, name, manifest: JSON.stringify(manifest), enabled: false, installed: true });

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

// Rust keeps the flag on when this machine failed to start the plugin, and the next launch loads
// it: a card left off would tell the user the opposite of what happens.
test("a failed enable shows the switch the row was left with, and why", async () => {
	console.error = () => {};
	for (const [url, kept] of [
		[KEPT_URL, true],
		[OFF_URL, false],
	] as const) {
		listed = { rows: [{ url, enabled: kept }] };
		const plugin = listedPlugin(url, `Row-${kept}`);
		await plugin.enable();
		await settle();
		expect(plugin.enabled).toBe(kept);
		expect(plugin.loadError._).toBe(undelivered);
	}
});

// A row read still in flight when the user turned the plugin off must not switch it back on.
test("a row read answered after a later stop leaves the card as the stop left it", async () => {
	console.error = () => {};
	let release!: () => void;
	listed = { rows: [{ url: RACED_URL, enabled: true }], gate: new Promise<void>((r) => (release = r)) };
	const plugin = listedPlugin(RACED_URL, "Raced");
	await plugin.enable();
	await plugin.disable();
	release();
	await settle();
	expect(plugin.enabled).toBe(false);
});

// A dependency left on but not running would let its dependant start against code that never ran.
test("a dependant is not started while its dependency is not running", async () => {
	console.error = () => {};
	listed = { rows: [{ url: DEP_URL, enabled: true }] };
	// Left on by a start this machine could not finish, as the load pass reports it.
	LunaPlugin.fromPluginInfo({ url: DEP_URL, name: "DepStopped", manifest: "{}", enabled: true, installed: true });
	LunaPlugin.getByUrl(DEP_URL)!.loadError._ = undelivered;
	const dependant = listedPlugin(DEPENDANT_URL, "Dependant", {
		name: "Dependant",
		luna: { dependencies: [{ name: "DepStopped", storeUrl: "https://x" }] },
	});
	await dependant.enable();
	await settle();
	expect(enableCalls).toEqual([DEP_URL]);
	expect(dependant.loadError._).toContain("DepStopped");
});
