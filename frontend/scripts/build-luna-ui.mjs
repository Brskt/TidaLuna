/**
 * Build @luna/ui plugin and generate an inline module for the main bundle.
 *
 * 1. Runs esbuild to produce luna-ui.mjs
 * 2. Reads the output and writes src/plugins/luna-ui-inline.ts
 *    which exports the code as a string constant
 */
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { readFileSync, writeFileSync, mkdirSync } from "fs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");
const uiDir = resolve(root, "plugins/ui");

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

const dynamicExternalsPlugin = {
    name: "dynamic-externals",
    setup(build) {
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

// Step 1: Build luna-ui.mjs
const outfile = resolve(uiDir, "dist/luna-ui.mjs");
mkdirSync(resolve(uiDir, "dist"), { recursive: true });

await build({
    entryPoints: [resolve(uiDir, "src/index.tsx")],
    bundle: true,
    format: "esm",
    platform: "browser",
    outfile,
    minify: true,
    treeShaking: true,
    jsx: "automatic",
    jsxImportSource: "react",
    alias: {
        "plugins/lib.native/src/index.native": resolve(
            root, "plugins/lib.native/src/index.native.ts"
        ),
    },
    plugins: [typeOnlyPlugin, dynamicExternalsPlugin],
    supported: { "top-level-await": true },
    logLevel: "info",
});

// Step 2: Read output and generate inline TS module
const code = readFileSync(outfile, "utf-8");
const escaped = JSON.stringify(code);

const inlineModule = `// AUTO-GENERATED — do not edit. Run: node scripts/build-luna-ui.mjs
export const LUNA_UI_CODE = ${escaped};
`;

writeFileSync(resolve(root, "src/plugins/luna-ui-inline.ts"), inlineModule);
console.log(`Generated src/plugins/luna-ui-inline.ts (${(code.length / 1024).toFixed(1)} KB)`);
