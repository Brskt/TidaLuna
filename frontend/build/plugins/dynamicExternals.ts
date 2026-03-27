import type { Plugin } from "esbuild";

/**
 * Resolves external modules to `window.require("module")` at runtime.
 * Used by inline plugin builds (@luna/ui, @luna/dev) so their imports
 * are satisfied by the modules registered on `window.require`.
 */
export const dynamicExternalsPlugin = (externals: string[]): Plugin => ({
	name: "dynamic-externals",
	setup(build) {
		const filter = new RegExp(
			"^(" + externals.map((e) => e.replace("/", "\\/")).join("|") + ")$"
		);
		build.onResolve({ filter }, (args) => ({
			path: args.path,
			namespace: "luna-external",
		}));
		build.onLoad({ filter: /.*/, namespace: "luna-external" }, (args) => ({
			contents: `module.exports = window.require(${JSON.stringify(args.path)});`,
			loader: "js",
		}));
	},
});
