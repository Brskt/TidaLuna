/**
 * esbuild config for @luna/ui plugin.
 *
 * Bundles MUI, Emotion, semver, @inrixia/helpers, material-ui-confirm INTO the output.
 * External modules (@luna/core, @luna/lib, oby, react, react-dom/client, react/jsx-runtime)
 * are resolved at runtime via window.require() → luna.core.modules["name"].
 */
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));

// Modules resolved at runtime via window.require()
const EXTERNALS = [
    "@luna/core",
    "@luna/lib",
    "oby",
    "react",
    "react-dom/client",
    "react/jsx-runtime",
];

// Type-only packages — resolve to empty module (no JS at runtime)
const TYPE_ONLY_PACKAGES = [
    "@octokit/openapi-types",
];

const typeOnlyPlugin = {
    name: "type-only-empty",
    setup(build) {
        const filter = new RegExp(
            "^(" + TYPE_ONLY_PACKAGES.join("|").replace("/", "\\/") + ")$"
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
};

/**
 * esbuild plugin that converts external imports to CommonJS-style runtime lookups:
 *   const mod = window.require("@luna/core");
 *   module.exports = mod;
 *
 * This matches TidaLuna's dynamicExternalsPlugin behavior.
 */
const dynamicExternalsPlugin = {
    name: "dynamic-externals",
    setup(build) {
        // Mark external modules so esbuild doesn't try to resolve them
        const filter = new RegExp(
            "^(" + EXTERNALS.map((e) => e.replace("/", "\\/")).join("|") + ")$"
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
};

await build({
    entryPoints: [resolve(__dirname, "src/index.tsx")],
    bundle: true,
    format: "esm",
    platform: "browser",
    outfile: resolve(__dirname, "dist/luna-ui.mjs"),
    minify: true,
    treeShaking: true,
    jsx: "automatic",
    jsxImportSource: "react",
    alias: {
        // Map the monorepo-relative import to our local stub
        "plugins/lib.native/src/index.native": resolve(
            __dirname,
            "../lib.native/src/index.native.ts"
        ),
    },
    plugins: [typeOnlyPlugin, dynamicExternalsPlugin],
    // Silence warnings about top-level await (used in Storage.tsx, PluginStoreTab/index.tsx)
    supported: {
        "top-level-await": true,
    },
    logLevel: "info",
});
