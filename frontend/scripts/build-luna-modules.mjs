/**
 * Build @luna/core and @luna/lib as standalone ESM bundles for the rquickjs runtime.
 *
 * Each bundle resolves all internal (relative) imports but externalizes
 * dependencies that are provided separately in the QuickJS module loader:
 *   - react, react-dom/client, react/jsx-runtime  (stubs)
 *   - oby, @inrixia/helpers                       (bundled from node_modules)
 *   - idb-keyval                                   (shim → SQLite)
 *
 * Output:
 *   frontend/dist/luna-core.mjs   — @luna/core
 *   frontend/dist/luna-lib.mjs    — @luna/lib
 *   frontend/dist/oby.mjs         — oby (reactive store)
 *   frontend/dist/inrixia-helpers.mjs — @inrixia/helpers
 */
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");

// --- QuickJS override plugin ---
// Replaces browser-only luna-core modules with QuickJS-compatible stubs.
// Only affects the @luna/core bundle — the CEF bundle (build-bundle.mjs) is unchanged.
const overridesDir = resolve(root, "src/quickjs-overrides");
const quickjsOverrides = {
    name: "quickjs-overrides",
    setup(build) {
        // Map original absolute paths → override file paths
        const lunaCoreDir = resolve(root, "src/luna-core");
        const overrides = new Map([
            [resolve(lunaCoreDir, "exposeTidalInternals.ts"), resolve(overridesDir, "exposeTidalInternals.ts")],
            [resolve(lunaCoreDir, "loadingContainer.ts"), resolve(overridesDir, "loadingContainer.ts")],
            [resolve(lunaCoreDir, "window.core.ts"), resolve(overridesDir, "window.core.ts")],
            [resolve(lunaCoreDir, "modules.ts"), resolve(overridesDir, "modules.ts")],
        ]);
        // Intercept file loading: when esbuild loads an original file, serve the override instead
        const filter = /\/(exposeTidalInternals|loadingContainer|window\.core|modules)\.ts$/;
        build.onLoad({ filter }, async (args) => {
            const override = overrides.get(args.path);
            if (!override) return;
            const { readFile } = await import("fs/promises");
            return { contents: await readFile(override, "utf-8"), loader: "ts" };
        });
        // Resolve @luna/core self-references to index.ts
        build.onResolve({ filter: /^@luna\/core$/ }, () => ({
            path: resolve(lunaCoreDir, "index.ts"),
        }));
    },
};

// Dependencies provided separately in the rquickjs module loader
const CORE_EXTERNALS = [
    "@inrixia/helpers",
    "react",
    "react-dom/client",
    "react/jsx-runtime",
    "oby",
    "idb-keyval",
];

const LIB_EXTERNALS = [
    "@luna/core",
    "@inrixia/helpers",
];

// --- @luna/core ---
await build({
    entryPoints: [resolve(root, "src/luna-core/index.ts")],
    outfile: resolve(root, "dist/luna-core.mjs"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "esnext",
    treeShaking: true,
    external: CORE_EXTERNALS,
    plugins: [quickjsOverrides],
    logLevel: "info",
});
console.log("  luna-core.mjs built");

// --- @luna/lib ---
await build({
    entryPoints: [resolve(root, "src/luna-lib/index.ts")],
    outfile: resolve(root, "dist/luna-lib.mjs"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "esnext",
    treeShaking: true,
    external: LIB_EXTERNALS,
    alias: {
        "@luna/core": "@luna/core",
    },
    logLevel: "info",
});
console.log("  luna-lib.mjs built");

// --- oby (reactive store, used by ReactiveStore & LunaPlugin) ---
await build({
    entryPoints: [resolve(root, "node_modules/oby/dist/index.js")],
    outfile: resolve(root, "dist/oby.mjs"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "esnext",
    treeShaking: true,
    logLevel: "info",
});
console.log("  oby.mjs built");

// --- @inrixia/helpers (Semaphore, Signal, memoize, etc.) ---
await build({
    entryPoints: [resolve(root, "node_modules/@inrixia/helpers/dist/index.js")],
    outfile: resolve(root, "dist/inrixia-helpers.mjs"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "esnext",
    treeShaking: true,
    logLevel: "info",
})
console.log("  inrixia-helpers.mjs built");
