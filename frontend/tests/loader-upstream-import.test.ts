// Tests that `src/plugins/loader.ts` shuts upstream's own way of running a plugin: importing its code
// into the page, past the gate and the token purge Rust holds every plugin to.

import { expect, test } from "bun:test";

(window as any).luna ??= {};
const { LunaPlugin: CoreLunaPlugin } = await import("../render/src");
await import("../src/plugins/loader");

test("no upstream plugin instance imports code into the page", async () => {
	const created = URL.createObjectURL;
	let imports = 0;
	URL.createObjectURL = ((blob: Blob) => {
		imports++;
		return created.call(URL, blob);
	}) as any;
	const plugin = new (CoreLunaPlugin as any)("planted", {
		url: "https://x/planted",
		enabled: true,
		installed: true,
		liveReload: false,
		hideSettings: false,
		package: { name: "planted", code: "globalThis.__planted = 1; export {}" },
	});

	try {
		await expect(plugin.loadExports()).rejects.toThrow();
	} finally {
		URL.createObjectURL = created;
	}

	expect(imports).toBe(0);
	expect((globalThis as any).__planted).toBeUndefined();
});
