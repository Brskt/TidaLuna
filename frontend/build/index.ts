import { type BuildOptions, build as esBuild } from "esbuild";
import { readFileSync } from "fs";
import { resolve } from "path";

export const defaultBuildOptions: BuildOptions = {
	bundle: true,
	minify: true,
	treeShaking: true,
	target: "esnext",
};

/** Modules resolved at runtime via window.require() - shared by UI and dev inline builds. */
export const RUNTIME_EXTERNALS = [
	"@luna/core",
	"@luna/lib",
	"@luna/ui",
	"oby",
	"react",
	"react-dom/client",
	"react/jsx-runtime",
];

/** Packages that only export TypeScript types - no JS at runtime. */
export const TYPE_ONLY_PACKAGES = [
	"@octokit/openapi-types",
];

/** Read `luna.dependencies` from a package.json and return their names as externals. */
export const getLunaDependencyExternals = (packageJsonPath: string): string[] => {
	const pkg = JSON.parse(readFileSync(resolve(packageJsonPath), "utf-8"));
	return (pkg.luna?.dependencies ?? [])
		.map((d: { name?: string } | string) => (typeof d === "string" ? d : d?.name))
		.filter((n: unknown): n is string => typeof n === "string" && (n as string).length > 0);
};

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
