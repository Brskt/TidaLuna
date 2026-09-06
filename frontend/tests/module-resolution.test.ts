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
const hostDomFull = { createRoot: () => {}, createPortal: () => {} };
// What TIDAL has served since the 2026-08-26 build: react-dom without createRoot.
const hostDomSplit = { createPortal: () => {}, flushSync: () => {} };

const store = {} as never;

const captureAll = () => {
	(window as any).__lunaHostModules = {
		react: hostReact,
		"react/jsx-runtime": hostJsx,
		"react-dom/client": hostDomSplit,
	};
};

beforeEach(() => {
	for (const key of Object.keys(modules)) delete modules[key];
	delete (globalThis as any).__lunaCreateRoot;
	(window as any).__lunaHostModules = {};
});

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

// The pin must not reach further than the host's own modules. `LunaPlugin` writes
// `modules[this.name]` on load and deletes it on unload, and the core-plugin names
// (`@luna/lib`, `@luna/lib.native`, `@luna/ui`, `@luna/dev`, `@luna/linux`) go through that
// same path, and pinning one of those would break loading it.
test("a plugin's own slot stays writable and removable", () => {
	modules["@some/plugin"] = { exports: 1 };
	modules["@some/plugin"] = { exports: 2 };
	expect(modules["@some/plugin"]).toEqual({ exports: 2 });

	delete modules["@some/plugin"];
	expect(modules["@some/plugin"]).toBeUndefined();
});

test("uses the host trio when the host supplies all three", () => {
	(window as any).__lunaHostModules = {
		react: hostReact,
		"react/jsx-runtime": hostJsx,
		"react-dom/client": hostDomFull,
	};
	initModules(store);
	expect(modules["react"]).toBe(hostReact);
	expect(modules["react/jsx-runtime"]).toBe(hostJsx);
	expect(modules["react-dom/client"]).toBe(hostDomFull);
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
	captureAll();
	initModules(store);
	expect(modules["react"]).not.toBe(hostReact);
	expect(modules["react/jsx-runtime"]).not.toBe(hostJsx);
	expect(modules["react-dom/client"]).not.toBe(hostDomSplit);
	expect(typeof modules["react"].useState).toBe("function");
	expect(typeof modules["react-dom/client"].createRoot).toBe("function");
});

test("demotes all three when nothing was captured at all", () => {
	initModules(store);
	expect(typeof modules["react"].useState).toBe("function");
	expect(typeof modules["react/jsx-runtime"].jsx).toBe("function");
	expect(typeof modules["react-dom/client"].createRoot).toBe("function");
});

test("ignores a tagged createRoot that is not callable", () => {
	(globalThis as any).__lunaCreateRoot = "not a function";
	captureAll();
	initModules(store);
	expect(modules["react"]).not.toBe(hostReact);
});
