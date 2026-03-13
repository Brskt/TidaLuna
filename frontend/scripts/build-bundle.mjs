/**
 * Build the main frontend bundle with esbuild.
 * Replaces `bun build` so the entire build runs via `node`.
 */
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");

await build({
    entryPoints: [resolve(root, "src/index.ts")],
    outfile: resolve(root, "dist/bundle.js"),
    bundle: true,
    minify: true,
    format: "iife",
    platform: "browser",
    target: "esnext",
    define: { "process.env.NODE_ENV": '"production"' },
    alias: {
        "@luna/core": resolve(root, "src/luna-core/index.ts"),
        "@luna/lib": resolve(root, "src/luna-lib/index.ts"),
    },
});

console.log("  bundle.js built with esbuild");
