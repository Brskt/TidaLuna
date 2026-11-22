// Tests Reload all in `src/plugins/luna-plugin.ts` against a fake Rust holding its two guards:
// an unload is refused while a running plugin depends on the target, an enable needs every
// dependency enabled. One plugin's Reload after another trips the first guard.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";

import { leafFirstGroups } from "../src/plugins/reload-order";

type Row = { name: string; enabled: boolean; live: boolean; deps: string[] };

const rows = new Map<string, Row>();
const calls: string[] = [];
const pulls = new Map<string, () => Promise<unknown>>();
const failStart = new Set<string>();
let onPull: ((url: string) => void) | undefined;

const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		const url = args[0] as string;
		const row = rows.get(url);
		switch (channel) {
			case "plugin.update": {
				onPull?.(url);
				const pull = pulls.get(url);
				return pull ? pull() : { outcome: "upToDate", hash: "h" };
			}
			case "plugin.disable": {
				calls.push(`disable ${row!.name}`);
				row!.enabled = false;
				row!.live = false;
				return true;
			}
			case "plugin.unload": {
				calls.push(`unload ${row!.name}`);
				const running = [...rows.values()].filter((r) => r.live && r.deps.includes(row!.name));
				if (running.length > 0) {
					throw new Error(`Cannot stop '${row!.name}': running dependants: ${running.map((r) => r.name).join(", ")}`);
				}
				row!.live = false;
				return true;
			}
			case "plugin.enable": {
				calls.push(`enable ${row!.name}`);
				const off = row!.deps.filter((d) => ![...rows.values()].some((r) => r.name === d && r.enabled));
				if (off.length > 0) throw new Error(`Cannot enable: deps not satisfied: ${off.join(", ")}`);
				if (failStart.has(url)) throw new Error("The page did not take the plugin in time");
				row!.enabled = true;
				row!.live = true;
				return true;
			}
			case "plugin.list":
				return [...rows].map(([u, r]) => ({ url: u, name: r.name, enabled: r.enabled, installed: true }));
		}
		return ipc.invokeIpc(channel, ...args);
	},
}));

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

const add = (name: string, deps: string[] = []) => {
	const url = `https://x/${name}`;
	rows.set(url, { name, enabled: true, live: true, deps });
	const manifest = { name, luna: { dependencies: deps.map((d) => ({ name: d, storeUrl: "" })) } };
	return LunaPlugin.fromPluginInfo({ url, name, manifest: JSON.stringify(manifest), enabled: true, installed: true });
};

beforeEach(() => {
	for (const url of Object.keys(LunaPlugin.plugins)) delete LunaPlugin.plugins[url];
	rows.clear();
	calls.length = 0;
	pulls.clear();
	failStart.clear();
	onPull = undefined;
});
afterEach(() => {
	for (const url of Object.keys(LunaPlugin.plugins)) delete LunaPlugin.plugins[url];
});

test("groups list dependants first and keep unrelated plugins apart", () => {
	const node = (name: string, dependencies: string[] = []) => ({ url: name, name, dependencies });
	expect(leafFirstGroups([node("lib"), node("app", ["lib"]), node("solo")])).toEqual([["app", "lib"], ["solo"]]);
	// Diamond: both middles before the base, the top before both.
	expect(leafFirstGroups([node("base"), node("left", ["base"]), node("right", ["base"]), node("top", ["left", "right"])]))
		.toEqual([["top", "left", "right", "base"]]);
	// A dependency outside the set ties nothing; a cycle comes out whole.
	expect(leafFirstGroups([node("a", ["missing"]), node("b")])).toEqual([["a"], ["b"]]);
	expect(leafFirstGroups([node("x", ["y"]), node("y", ["x"])])).toEqual([["x", "y"]]);
});

test("a chain restarts: dependants stop first, dependencies start first", async () => {
	add("lib");
	add("mid", ["lib"]);
	add("app", ["mid"]);

	const result = await LunaPlugin.reloadAll();

	expect(calls).toEqual(["unload app", "unload mid", "unload lib", "enable lib", "enable mid", "enable app"]);
	expect(result.notReloaded).toEqual([]);
	expect(result.reloaded.sort()).toEqual(["app", "lib", "mid"]);
	expect([...rows.values()].every((r) => r.enabled)).toBe(true);
});

test("each group restarts before the next unloads: an unloaded plugin is not running until its start", async () => {
	add("one");
	add("two");

	await LunaPlugin.reloadAll();

	expect(calls).toEqual(["unload one", "enable one", "unload two", "enable two"]);
});

test("a busy dependant stays out, its dependency keeps running, the others restart", async () => {
	add("lib");
	const busy = add("busy", ["lib"]);
	add("app", ["lib"]);
	pulls.set(busy.url, async () => ({ outcome: "busy" }));

	const result = await LunaPlugin.reloadAll();

	expect(calls).toEqual(["unload app", "unload lib", "enable app"]);
	expect(result.reloaded).toEqual(["app"]);
	expect(result.notReloaded.map((p) => p.name).sort()).toEqual(["busy", "lib"]);
	expect(result.notReloaded.find((p) => p.name === "lib")!.reason).toStartWith("Not restarted: Cannot stop 'lib'");
	expect([...rows.values()].every((r) => r.enabled)).toBe(true);
});

test("a plugin turned off during the pulls is not turned back on", async () => {
	const first = add("first");
	const user = add("user");
	onPull = (url) => {
		if (url === first.url) {
			rows.get(user.url)!.enabled = false;
			user.enabled = false;
		}
	};

	const result = await LunaPlugin.reloadAll();

	expect(calls).toEqual(["unload first", "enable first"]);
	expect(rows.get(user.url)!.enabled).toBe(false);
	expect(result.notReloaded).toEqual([{ name: "user", reason: "Turned off or removed while reloading" }]);
});

test("a dependency that fails to start is reported once, from the final state", async () => {
	const lib = add("lib");
	add("app", ["lib"]);
	failStart.add(lib.url);

	const result = await LunaPlugin.reloadAll();

	expect(result.reloaded).toEqual([]);
	const reasons = Object.fromEntries(result.notReloaded.map((p) => [p.name, p.reason]));
	expect(reasons.lib).toBe("The page did not take the plugin in time");
	expect(reasons.app).toBe("Dependency 'lib' is not running");
});

test("a second call joins the run in flight", async () => {
	add("only");
	let release!: () => void;
	pulls.set("https://x/only", () => new Promise((resolve) => (release = () => resolve({ outcome: "upToDate", hash: "h" }))));

	const first = LunaPlugin.reloadAll();
	const second = LunaPlugin.reloadAll();
	expect(second).toBe(first);
	expect(LunaPlugin.reloadAllInFlight).toBe(first);
	await new Promise((resolve) => setTimeout(resolve, 0));
	release();
	await first;

	expect(calls).toEqual(["unload only", "enable only"]);
	expect(LunaPlugin.reloadAllInFlight).toBeUndefined();
});

test("a new version that declares a dependency is ordered by it", async () => {
	const lib = add("lib");
	const app = add("app");
	const manifest = JSON.stringify({ name: "app", luna: { dependencies: [{ name: "lib", storeUrl: "" }] } });
	pulls.set(app.url, async () => {
		rows.get(app.url)!.deps = ["lib"];
		return { outcome: "updated", hash: "h2", manifest };
	});

	const result = await LunaPlugin.reloadAll();

	expect(calls).toEqual(["unload app", "unload lib", "enable lib", "enable app"]);
	expect(result.notReloaded).toEqual([]);
	expect(lib.running && app.running).toBe(true);
});

test("plugins stay busy for their card until the run ends", async () => {
	const only = add("only");
	let release!: () => void;
	pulls.set(only.url, () => new Promise((resolve) => (release = () => resolve({ outcome: "upToDate", hash: "h" }))));

	const run = LunaPlugin.reloadAll();
	expect(only.loading._).toBe(true);
	await new Promise((resolve) => setTimeout(resolve, 0));
	release();
	await run;

	expect(only.loading._).toBe(false);
});
