/**
 * Centralized esbuild configuration - single entry point for all frontend builds.
 *
 * Build order is strict and sequential:
 *   Phase 1: Inline plugins (luna-ui, luna-dev) - generates .ts string constants
 *   Phase 2: Main bundle (bundle.js) - imports the inline .ts from Phase 1
 *   Phase 3: Standalone ESM modules (luna-core, luna-lib, oby, helpers)
 */
import { type BuildOptions } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { mkdirSync, readFileSync } from "fs";
import { build, defaultBuildOptions, RUNTIME_EXTERNALS, TYPE_ONLY_PACKAGES, getLunaDependencyExternals } from "./build";
import { dynamicExternalsPlugin } from "./build/plugins/dynamicExternals";
import { typeOnlyPlugin } from "./build/plugins/typeOnly";
import { coreSelfResolvePlugin } from "./build/plugins/coreSelfResolve";
import { inlineBundlePlugin } from "./build/plugins/inlineBundle";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = __dirname;

// Read version from Cargo.toml (single source of truth)
const cargoToml = readFileSync(resolve(root, "../Cargo.toml"), "utf-8");
const appVersion = cargoToml.match(/^version\s*=\s*"(.+)"/m)?.[1] ?? "unknown";

mkdirSync(resolve(root, "dist"), { recursive: true });
mkdirSync(resolve(root, "plugins/ui/dist"), { recursive: true });
mkdirSync(resolve(root, "plugins/dev/dist"), { recursive: true });

// Shared plugin configs for inline builds
const inlinePluginDefaults: Partial<BuildOptions> = {
	...defaultBuildOptions,
	format: "esm",
	platform: "browser",
	jsx: "automatic",
	jsxImportSource: "react",
	supported: { "top-level-await": true },
	define: { __TIDALUNAR_VERSION__: JSON.stringify(appVersion) },
};

const uiOutfile = resolve(root, "plugins/ui/dist/luna-ui.mjs");
const devOutfile = resolve(root, "plugins/dev/dist/luna-dev.mjs");

// ─── Phase 1: Inline plugins ────────────────────────────────────────────
// Must run BEFORE bundle.js - src/index.ts imports the generated inline .ts files.

// Merge RUNTIME_EXTERNALS with any luna.dependencies declared in plugin package.json files
const uiExternals = [...new Set([...RUNTIME_EXTERNALS, ...getLunaDependencyExternals(resolve(root, "plugins/ui/package.json"))])];
const devExternals = [...new Set([...RUNTIME_EXTERNALS, ...getLunaDependencyExternals(resolve(root, "plugins/dev/package.json"))])];

const lunaUiInline: BuildOptions = {
	...inlinePluginDefaults,
	entryPoints: [resolve(root, "plugins/ui/src/index.tsx")],
	outfile: uiOutfile,
	alias: {
		"plugins/lib.native/src/index.native": resolve(root, "plugins/lib.native/src/index.native.ts"),
	},
	plugins: [
		dynamicExternalsPlugin(uiExternals),
		typeOnlyPlugin(TYPE_ONLY_PACKAGES),
		inlineBundlePlugin({
			outfile: uiOutfile,
			inlinePath: resolve(root, "src/plugins/luna-ui-inline.ts"),
			exportName: "LUNA_UI_CODE",
		}),
	],
	logLevel: "info",
};

const lunaDevInline: BuildOptions = {
	...inlinePluginDefaults,
	entryPoints: [resolve(root, "plugins/dev/src/index.ts")],
	outfile: devOutfile,
	plugins: [
		dynamicExternalsPlugin(devExternals),
		typeOnlyPlugin(TYPE_ONLY_PACKAGES),
		inlineBundlePlugin({
			outfile: devOutfile,
			inlinePath: resolve(root, "src/plugins/luna-dev-inline.ts"),
			exportName: "LUNA_DEV_CODE",
		}),
	],
	logLevel: "info",
};

// ─── Phase 2: Main bundle ───────────────────────────────────────────────
// Depends on Phase 1 - bundles src/index.ts which imports luna-ui-inline.ts and luna-dev-inline.ts.

const mainBundle: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "src/index.ts")],
	outfile: resolve(root, "dist/bundle.js"),
	format: "iife",
	platform: "browser",
	define: { "process.env.NODE_ENV": '"production"' },
	alias: {
		"@luna/core": resolve(root, "render/src/index.ts"),
		"@luna/lib": resolve(root, "plugins/lib/src/index.ts"),
	},
};

// ─── Phase 3: Standalone ESM modules ────────────────────────────────────
// Independent modules used at runtime by plugins via window.require().

const CORE_EXTERNALS = [
	"@inrixia/helpers",
	"react",
	"react-dom/client",
	"react/jsx-runtime",
	"oby",
	"idb-keyval",
];

const lunaCore: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "render/src/index.ts")],
	outfile: resolve(root, "dist/luna-core.mjs"),
	format: "esm",
	platform: "neutral",
	external: CORE_EXTERNALS,
	plugins: [coreSelfResolvePlugin(resolve(root, "render/src"))],
	logLevel: "info",
};

const lunaLib: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "plugins/lib/src/index.ts")],
	outfile: resolve(root, "dist/luna-lib.mjs"),
	format: "esm",
	platform: "neutral",
	external: ["@luna/core", "@inrixia/helpers"],
	alias: { "@luna/core": "@luna/core" },
	logLevel: "info",
};

const oby: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "node_modules/oby/dist/index.js")],
	outfile: resolve(root, "dist/oby.mjs"),
	format: "esm",
	platform: "neutral",
	logLevel: "info",
};

const inrixiaHelpers: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "node_modules/@inrixia/helpers/dist/index.js")],
	outfile: resolve(root, "dist/inrixia-helpers.mjs"),
	format: "esm",
	platform: "neutral",
	logLevel: "info",
};

// ─── Execute ────────────────────────────────────────────────────────────
// Strict sequential order: Phase 1 → Phase 2 → Phase 3.

await build([
	lunaUiInline,
	lunaDevInline,
	mainBundle,
	lunaCore,
	lunaLib,
	oby,
	inrixiaHelpers,
]);

console.log("\nAll builds complete.");
