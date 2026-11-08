// Tests for `plugins/ui/src/SettingsPage/PluginStoreTab/pluginInstall.ts`.
//
// Mocked: `@luna/core` exists only in the esbuild bundle. Its registry is a plain record here, as
// the upstream one is; the confirm dialog and the store list are UI this test does not reach.

import { expect, mock, test } from "bun:test";

const URL_ = "https://new/Mover";
const plugins: Record<string, any> = {};
const quiet = () => {};

mock.module("@luna/core", () => ({
	LunaPlugin: { plugins, getByName: () => undefined },
	Tracer: () => ({ trace: { msg: { log: quiet, err: Object.assign(quiet, { withContext: () => quiet }) } } }),
}));
mock.module("../plugins/ui/src/helpers/confirm", () => ({ confirm: async () => {} }));
mock.module("../plugins/ui/src/SettingsPage/PluginStoreTab/storeState", () => ({ addToStores: quiet, storeUrls: [] }));

const { installPluginWithLibraries } = await import("../plugins/ui/src/SettingsPage/PluginStoreTab/pluginInstall");

const plugin = (installed: boolean) => {
	const installs: string[] = [];
	return { name: "Mover", url: URL_, installed, dependencyRequirements: [], installs, install: async () => void installs.push(URL_) };
};

// An Install started on a Store placeholder, whose dialogs straddle a store move, must not
// reinstall the moved plugin over the user's (turned off, its native trust reset).
test("an Install held from before a move does not reinstall the moved plugin", async () => {
	const placeholder = plugin(false);
	const moved = plugin(true);
	plugins[URL_] = moved;

	expect(await installPluginWithLibraries(placeholder as any)).toBe(true);
	expect(placeholder.installs).toEqual([]);
	expect(moved.installs).toEqual([]);
});
