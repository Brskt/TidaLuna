import type { Plugin } from "esbuild";

/**
 * Resolves type-only packages to an empty module.
 * These packages export only TypeScript types - no JS at runtime.
 */
export const typeOnlyPlugin = (packages: string[]): Plugin => ({
	name: "type-only-empty",
	setup(build) {
		const filter = new RegExp(
			"^(" + packages.join("|").replace("/", "\\/") + ")$"
		);
		build.onResolve({ filter }, (args) => ({
			path: args.path,
			namespace: "type-only",
		}));
		build.onLoad({ filter: /.*/, namespace: "type-only" }, () => ({
			contents: "export default {};",
			loader: "js",
		}));
	},
});
