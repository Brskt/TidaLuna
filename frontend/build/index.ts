import { type BuildOptions, build as esBuild } from "esbuild";

export const defaultBuildOptions: BuildOptions = {
	bundle: true,
	minify: true,
	treeShaking: true,
	target: "esnext",
};

/** Modules resolved at runtime via window.require() — shared by UI and dev inline builds. */
export const RUNTIME_EXTERNALS = [
	"@luna/core",
	"@luna/lib",
	"@luna/ui",
	"oby",
	"react",
	"react-dom/client",
	"react/jsx-runtime",
];

/** Packages that only export TypeScript types — no JS at runtime. */
export const TYPE_ONLY_PACKAGES = [
	"@octokit/openapi-types",
];

/**
 * Execute esbuild configs **strictly sequentially**.
 * Order matters: inline plugins must be generated before bundle.js
 * because src/index.ts imports the inline .ts files.
 */
export const build = async (configs: BuildOptions[]) => {
	for (const config of configs) {
		await esBuild(config);
	}
};
