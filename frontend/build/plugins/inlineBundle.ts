import type { Plugin } from "esbuild";
import { readFileSync, writeFileSync } from "fs";

/**
 * Post-build plugin: reads the built .mjs output and generates a .ts file
 * that exports the code as a string constant. This allows the main bundle
 * to embed plugin code inline without a separate network fetch.
 */
export const inlineBundlePlugin = (opts: {
	outfile: string;
	inlinePath: string;
	exportName: string;
}): Plugin => ({
	name: "inline-bundle",
	setup(build) {
		build.onEnd(() => {
			const code = readFileSync(opts.outfile, "utf-8");
			const escaped = JSON.stringify(code);
			const module = `// AUTO-GENERATED - do not edit. Rebuild with: bun esbuild.config.ts\nexport const ${opts.exportName} = ${escaped};\n`;
			writeFileSync(opts.inlinePath, module);
			console.log(`  Generated ${opts.inlinePath} (${(code.length / 1024).toFixed(1)} KB)`);
		});
	},
});
