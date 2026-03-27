import type { Plugin } from "esbuild";
import { resolve } from "path";

/**
 * Resolves `@luna/core` self-references within the @luna/core package
 * to the actual source directory, preventing circular external resolution.
 */
export const coreSelfResolvePlugin = (coreDir: string): Plugin => ({
	name: "core-self-resolve",
	setup(build) {
		build.onResolve({ filter: /^@luna\/core$/ }, () => ({
			path: resolve(coreDir, "index.ts"),
		}));
	},
});
