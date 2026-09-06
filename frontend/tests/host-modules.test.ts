// Tests `src/ui/early_runtime/host_modules.js`, the sink the Rust capture filter appends
// `__LUNA_CAP(id, ns)` to. It had no coverage at all, which is how a policy that silently
// discarded half of a split module survived: the id assignment looked correct, and it is;
// nothing pointed at the writer.
//
// The policy is the point. One id can legitimately name several chunks (react-dom's
// exports were already split once), and replacing rather than unioning leaves a namespace
// that satisfies `initModules`' validator with an export missing. That failure surfaces
// far away, as a bare TypeError inside a plugin calling the export that got dropped.

import { beforeEach, expect, test } from "bun:test";

const source = await Bun.file(new URL("../../src/ui/early_runtime/host_modules.js", import.meta.url)).text();

type Cap = (id: string, ns: Record<string, unknown>) => void;

// A fresh copy of the script over a clean sink, as it runs in the page: before any chunk
// and before any plugin.
function load(): Cap {
	delete (window as any).__lunaHostModules;
	delete (globalThis as any).__LUNA_CAP;
	new Function(source)();
	return (globalThis as any).__LUNA_CAP as Cap;
}

const sink = () => (window as any).__lunaHostModules as Record<string, Record<string, unknown>>;

beforeEach(() => {
	delete (Object.prototype as any).createRoot;
	delete (Object.prototype as any).flushSync;
});

test("the first capture of an id lands whole", () => {
	const cap = load();
	const ns = { createPortal() {}, flushSync() {} };
	cap("react-dom/client", ns);
	expect(sink()["react-dom/client"]).toBe(ns);
});

test("a second chunk under one id adds its exports instead of being dropped", () => {
	// THE REGRESSION. These are the real shapes: since TIDAL's 2026-08 build the
	// `react-dom-*` chunk carries only createPortal/flushSync, and `createRoot` lives
	// elsewhere. Under first-writer-replaces, createRoot never arrives, `missing` is
	// empty because the validator's marker is present, and a plugin calling createRoot
	// takes a TypeError with nothing naming the cause.
	const cap = load();
	cap("react-dom/client", { createPortal() {}, flushSync() {} });
	cap("react-dom/client", { createRoot() {} });
	const got = sink()["react-dom/client"];
	expect(Object.keys(got).sort()).toEqual(["createPortal", "createRoot", "flushSync"]);
	expect(typeof got.createRoot).toBe("function");
});

test("the first writer still wins a name it already holds", () => {
	// What the original policy existed to protect: the real react chunk is modulepreloaded
	// and runs first, and a lazy vendor chunk must not take its exports over.
	const cap = load();
	const real = () => "real";
	cap("react", { createElement: real });
	cap("react", { createElement: () => "late" });
	expect(sink().react.createElement).toBe(real);
});

test("ids do not bleed into one another", () => {
	const cap = load();
	cap("react", { createElement() {} });
	cap("react/jsx-runtime", { jsx() {} });
	expect(Object.keys(sink().react)).toEqual(["createElement"]);
	expect(Object.keys(sink()["react/jsx-runtime"])).toEqual(["jsx"]);
});

test("a name a plugin left on Object.prototype is not captured as an export", () => {
	// A lazy chunk can execute after plugins have run: the merge walks own names only.
	// Enumerating with `for..in`, or testing with `in`, would read this as the namespace's
	// own export and hand a plugin's function to every consumer of the host React.
	const cap = load();
	cap("react-dom/client", { createPortal() {} });
	(Object.prototype as any).flushSync = () => "planted";
	cap("react-dom/client", { createRoot() {} });
	const got = sink()["react-dom/client"];
	expect(Object.keys(got).sort()).toEqual(["createPortal", "createRoot"]);
	expect(Object.getOwnPropertyDescriptor(got, "flushSync")).toBeUndefined();
});

test("an export the sink inherits rather than owns is still filled in", () => {
	// The mirror of the test above, and the reason the merge tests own descriptors instead
	// of `in`: a planted name must not make a real export look already present.
	const cap = load();
	cap("react-dom/client", { createPortal() {} });
	(Object.prototype as any).createRoot = () => "planted";
	const realRoot = () => "real";
	cap("react-dom/client", { createRoot: realRoot });
	expect(sink()["react-dom/client"].createRoot).toBe(realRoot);
});
