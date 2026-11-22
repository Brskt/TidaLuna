// Tests `LunaPlugin.install(url)` (via `luna.core.installPlugin`) against a fake Rust that replaces a
// reinstall's row itself and changes nothing when it refuses. The page must not turn a plugin off
// before that, nor undo a reinstall whose start fails: only a first install is cleaned up.

import { beforeEach, expect, mock, test } from "bun:test";

type Row = { name: string; enabled: boolean; live: boolean };

const rows = new Map<string, Row>();
const calls: string[] = [];
const installRefusals = new Map<string, string>();
const enableRefusals = new Map<string, string>();

const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		const url = args[0] as string;
		const name = url.split("/").pop()!;
		switch (channel) {
			case "plugin.install": {
				calls.push(`install ${name}`);
				const refused = installRefusals.get(url);
				if (refused) throw new Error(refused);
				rows.set(url, { name, enabled: false, live: false });
				return { url, name, manifest: JSON.stringify({ name }), hash: "new", enabled: false, installed: true };
			}
			case "plugin.enable": {
				calls.push(`enable ${name}`);
				const refused = enableRefusals.get(url);
				if (refused) throw new Error(refused);
				Object.assign(rows.get(url)!, { enabled: true, live: true });
				return true;
			}
			case "plugin.disable": {
				calls.push(`disable ${name}`);
				const row = rows.get(url);
				if (!row) throw new Error(`Plugin not found: ${url}`);
				Object.assign(row, { enabled: false, live: false });
				return true;
			}
			case "plugin.uninstall":
			case "plugin.cleanup_failed_install":
				calls.push(`${channel.slice(7)} ${name}`);
				rows.delete(url);
				return true;
			case "plugin.list":
				return [...rows].map(([u, r]) => ({ url: u, name: r.name, enabled: r.enabled, installed: true }));
		}
		return ipc.invokeIpc(channel, ...args);
	},
}));

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

// A plugin the user has, on and running.
const installed = (name: string) => {
	const url = `https://x/${name}`;
	rows.set(url, { name, enabled: true, live: true });
	return LunaPlugin.fromPluginInfo({ url, name, manifest: JSON.stringify({ name }), hash: "old", enabled: true, installed: true });
};

beforeEach(() => {
	for (const url of Object.keys(LunaPlugin.plugins)) delete LunaPlugin.plugins[url];
	rows.clear();
	calls.length = 0;
	installRefusals.clear();
	enableRefusals.clear();
	console.error = () => {};
});

test("a reinstall Rust refuses leaves the plugin the user had on and running", async () => {
	const kept = installed("Kept");
	installRefusals.set(kept.url, "The user declined the install");

	await expect(LunaPlugin.install(kept.url)).rejects.toThrow("The user declined the install");

	expect(calls).toEqual(["install Kept"]);
	expect(rows.get(kept.url)).toEqual({ name: "Kept", enabled: true, live: true });
	expect(LunaPlugin.plugins[kept.url]).toBe(kept);
	expect(kept.enabled).toBe(true);
});

test("a reinstall whose start fails is not uninstalled", async () => {
	const kept = installed("Kept");
	enableRefusals.set(kept.url, "Cannot enable: deps not satisfied: Lib");

	await expect(LunaPlugin.install(kept.url)).rejects.toThrow("deps not satisfied");
	await settle();

	expect(calls).toEqual(["install Kept", "enable Kept"]);
	expect(rows.has(kept.url)).toBe(true);
	expect(LunaPlugin.plugins[kept.url]).toBe(kept);
	expect(kept.installed).toBe(true);
	expect(kept.loadError._).toBe("Cannot enable: deps not satisfied: Lib");
});

test("a first install is undone when its start fails", async () => {
	const url = "https://x/Fresh";
	enableRefusals.set(url, "The page did not take the plugin in time");

	await expect(LunaPlugin.install(url)).rejects.toThrow("did not take the plugin");

	expect(calls).toEqual(["install Fresh", "enable Fresh", "uninstall Fresh"]);
	expect(rows.has(url)).toBe(false);
	expect(LunaPlugin.plugins[url]).toBeUndefined();
});
