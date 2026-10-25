/**
 * Centralized esbuild configuration - single entry point for all frontend builds.
 *
 * Built one at a time, in this order:
 *   Phase 1: Luna's plugins (luna-ui, luna-dev) as .mjs, served by luna_modules.rs
 *   Phase 2: Main bundle (bundle.js)
 *   Phase 3: Standalone ESM modules (luna-core, luna-lib, oby, helpers)
 * Then native-host.cjs, early-runtime/ (copied as is) and frontend.json are placed beside the
 * bundles: dist/ has the layout of the shipped frontend/ folder, which TIDALUNAR_FRONTEND_DIR
 * can point at.
 */
import { type BuildOptions } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { copyFileSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "fs";
import { build, defaultBuildOptions, RUNTIME_EXTERNALS, TYPE_ONLY_PACKAGES, getLunaDependencyExternals } from "./build";
import { dynamicExternalsPlugin } from "./build/plugins/dynamicExternals";
import { typeOnlyPlugin } from "./build/plugins/typeOnly";
import { coreSelfResolvePlugin } from "./build/plugins/coreSelfResolve";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = __dirname;

// Read version from Cargo.toml (single source of truth)
const cargoToml = readFileSync(resolve(root, "../Cargo.toml"), "utf-8");
const appVersion = cargoToml.match(/^version\s*=\s*"(.+)"/m)?.[1] ?? "unknown";
const versionDefine = { __TIDALUNAR_VERSION__: JSON.stringify(appVersion) };

mkdirSync(resolve(root, "dist"), { recursive: true });

// Shared config for Luna's own plugin builds (Phase 1)
const pluginDefaults: Partial<BuildOptions> = {
	...defaultBuildOptions,
	format: "esm",
	platform: "browser",
	jsx: "automatic",
	jsxImportSource: "react",
	supported: { "top-level-await": true },
	define: { ...versionDefine },
};

const uiOutfile = resolve(root, "dist/luna-ui.mjs");
const devOutfile = resolve(root, "dist/luna-dev.mjs");

// Phase 1, Luna's plugins (luna-ui, luna-dev).

// Merge RUNTIME_EXTERNALS with any luna.dependencies declared in plugin package.json files
const uiExternals = [...new Set([...RUNTIME_EXTERNALS, ...getLunaDependencyExternals(resolve(root, "plugins/ui/package.json"))])];
const devExternals = [...new Set([...RUNTIME_EXTERNALS, ...getLunaDependencyExternals(resolve(root, "plugins/dev/package.json"))])];

const lunaUi: BuildOptions = {
	...pluginDefaults,
	entryPoints: [resolve(root, "plugins/ui/src/index.tsx")],
	outfile: uiOutfile,
	alias: {
		"plugins/lib.native/src/index.native": resolve(root, "plugins/lib.native/src/index.native.ts"),
	},
	plugins: [
		dynamicExternalsPlugin(uiExternals),
		typeOnlyPlugin(TYPE_ONLY_PACKAGES),
	],
	logLevel: "info",
};

const lunaDev: BuildOptions = {
	...pluginDefaults,
	entryPoints: [resolve(root, "plugins/dev/src/index.ts")],
	outfile: devOutfile,
	plugins: [
		dynamicExternalsPlugin(devExternals),
		typeOnlyPlugin(TYPE_ONLY_PACKAGES),
	],
	logLevel: "info",
};

// Phase 2, main bundle (src/index.ts).

const mainBundle: BuildOptions = {
	...defaultBuildOptions,
	entryPoints: [resolve(root, "src/index.ts")],
	outfile: resolve(root, "dist/bundle.js"),
	format: "iife",
	platform: "browser",
	define: {
		"process.env.NODE_ENV": '"production"',
		...versionDefine,
	},
	alias: {
		"@luna/core": resolve(root, "render/src/index.ts"),
		"@luna/lib": resolve(root, "plugins/lib/src/index.ts"),
	},
};

// Phase 3, standalone ESM modules used at runtime by plugins via window.require().

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

await build([
	lunaUi,
	lunaDev,
	mainBundle,
	lunaCore,
	lunaLib,
	oby,
	inrixiaHelpers,
]);

// Emptied first so a script removed from the source cannot linger in dist/.
const earlyOut = resolve(root, "dist/early-runtime");
rmSync(earlyOut, { recursive: true, force: true });
mkdirSync(earlyOut);
for (const name of readdirSync(resolve(root, "early-runtime"))) {
	if (name.endsWith(".js")) copyFileSync(resolve(root, "early-runtime", name), resolve(earlyOut, name));
}
copyFileSync(resolve(root, "scripts/native-host.cjs"), resolve(root, "dist/native-host.cjs"));
// The binary refuses a shipped folder whose version differs from its own.
writeFileSync(resolve(root, "dist/frontend.json"), JSON.stringify({ version: appVersion }) + "\n");

console.log("\nAll builds complete.");
