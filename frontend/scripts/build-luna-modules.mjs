/**
 * Build standalone ESM bundles for modules that are used at runtime.
 *
 * These bundles are NOT for QuickJS anymore — they're used by the CEF frontend
 * as part of the main app bundle. The QuickJS overrides have been removed.
 *
 * Output:
 *   frontend/dist/luna-core.mjs   — @luna/core (no overrides, real browser code)
 *   frontend/dist/luna-lib.mjs    — @luna/lib
 *   frontend/dist/oby.mjs         — oby (reactive store)
 *   frontend/dist/inrixia-helpers.mjs — @inrixia/helpers
 */
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");

// Dependencies provided separately
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

// Resolve @luna/core self-references
const coreSelfResolve = {
    name: "core-self-resolve",
    setup(build) {
        const lunaCoreDir = resolve(root, "src/luna-core");
        build.onResolve({ filter: /^@luna\/core$/ }, () => ({
            path: resolve(lunaCoreDir, "index.ts"),
        }));
    },
};

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
    plugins: [coreSelfResolve],
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
