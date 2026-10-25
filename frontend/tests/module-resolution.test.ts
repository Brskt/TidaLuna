// Tests the React pairing invariant in `render/src/modules.ts`.
//
// React and its renderer have to be the same copy: react-dom writes the hook
// dispatcher onto the internals of whichever React it closed over. A host React
// driven by the bundled ReactDOM leaves that slot null and the first hook throws
// React #321, which empties the entire root, not one component. `initModules`
// therefore resolves the three React modules as a unit. It also completes a host
// react-dom that no longer carries `createRoot` (TIDAL's 2026-08 chunk split) from
// the global the Rust filter tags onto the entry chunk.

import { beforeEach, expect, test } from "bun:test";

import { defineHostModule, initModules, modules } from "../render/src/modules";

const hostReact = { createElement: () => {}, useState: () => {} };
const hostJsx = { jsx: () => {} };
// A react-dom without createRoot, the shape the fallback exists for (TIDAL's 2026-08-26 build).
const hostDomSplit = { createPortal: () => {}, flushSync: () => {} };

const store = {} as never;

const captureAll = () => {
	(window as any).__lunaHostModules = {
		react: hostReact,
		"react/jsx-runtime": hostJsx,
		"react-dom/client": hostDomSplit,
	};
};

const reset = () => {
	for (const key of Object.keys(modules)) delete modules[key];
	delete (globalThis as any).__lunaCreateRoot;
	(window as any).__lunaHostModules = {};
};

beforeEach(reset);

// A plugin's `@luna/lib` import is lowered to a LIVE lookup through this registry, which it
// reaches as `window.luna.core.modules`. While the slot was writable, the first plugin to run
// could wrap `__lunaLibFor` and be handed the capability of every plugin importing the lib
// after it, and then act as any of them on `plugin.storage.*`, `plugin.fetch` and
// `__Luna.registerNative`.
//
// Its own registry, because a pin is permanent by construction and the cases above wipe the
// live one between them.
test("a pinned host slot cannot be taken by a plugin", () => {
	const registry: Record<string, any> = {};
	const real = () => "host";
	defineHostModule("__lunaLibFor", real, registry);

	// The three shapes the takeover had: assign over it, redefine it, remove it.
	expect(() => {
		registry.__lunaLibFor = () => "stolen";
	}).toThrow();
	expect(() =>
		Object.defineProperty(registry, "__lunaLibFor", { value: () => "stolen" }),
	).toThrow();
	expect(() => {
		delete registry.__lunaLibFor;
	}).toThrow();

	expect(registry.__lunaLibFor).toBe(real);
});

test("completes a host react-dom that lost createRoot from the tagged global", () => {
	const tagged = () => {};
	(globalThis as any).__lunaCreateRoot = tagged;
	captureAll();
	initModules(store);
	expect(modules["react"]).toBe(hostReact);
	expect(modules["react/jsx-runtime"]).toBe(hostJsx);
	expect(modules["react-dom/client"].createRoot).toBe(tagged);
	// The rest of the host namespace survives the graft.
	expect(modules["react-dom/client"].flushSync).toBe(hostDomSplit.flushSync);
});

test("demotes all three to bundled when the renderer cannot be resolved", () => {
	// The regression this file exists for: react captured, react-dom/client not,
	// no tag. Resolving the three independently kept the host React and paired it
	// with a bundled ReactDOM, the one combination that cannot render.
	for (const [what, arrange] of [
		["react-dom/client without createRoot and no tag", captureAll],
		["nothing captured at all", () => {}],
		[
			"a tagged createRoot that is not callable",
			() => {
				(globalThis as any).__lunaCreateRoot = "not a function";
				captureAll();
			},
		],
	] as const) {
		reset();
		arrange();
		initModules(store);
		expect(modules["react"], what).not.toBe(hostReact);
		expect(modules["react/jsx-runtime"], what).not.toBe(hostJsx);
		expect(modules["react-dom/client"], what).not.toBe(hostDomSplit);
		expect(typeof modules["react"].useState, what).toBe("function");
		expect(typeof modules["react/jsx-runtime"].jsx, what).toBe("function");
		expect(typeof modules["react-dom/client"].createRoot, what).toBe("function");
	}
});
