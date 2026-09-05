import type { Store } from "redux";
import { coreTrace } from "./trace/Tracer";

// Bundled fallbacks - used when the host module wasn't captured (a 2nd React copy)
import * as React from "react";
import * as ReactDOMClient from "react-dom/client";
import * as jsxRuntime from "react/jsx-runtime";
import * as oby from "oby";

export const modules: Record<string, any> = {};

/**
 * Install a module the HOST owns, in a slot no plugin can take.
 *
 * Plugin code reaches this registry as `window.luna.core.modules`, and `transpile.rs` lowers
 * every `@luna/*` import to a live lookup through it, resolved when that plugin runs. A writable
 * slot therefore lets the first plugin to load answer for a module every later one imports;
 * `__lunaLibFor` is the sharp case, handing each plugin the capability its own IPC travels with.
 *
 * Pinned, not frozen: `Object.freeze(modules)` would also block the host's own later writes and
 * the slots plugins take under their own names. For the same reason the core-plugin names
 * (`@luna/lib`, `@luna/lib.native`, `@luna/ui`, `@luna/dev`, `@luna/linux`) must NOT come through
 * here, `LunaPlugin` owning those slots. `into` defaults to the live registry, where a pin
 * outlives the page.
 */
export function defineHostModule(name: string, value: any, into: Record<string, any> = modules): void {
	Object.defineProperty(into, name, {
		value,
		writable: false,
		configurable: false,
		enumerable: true,
	});
}

// Define a global require function to use modules for cjs imports bundled with esbuild
window.require = <NodeJS.Require>((moduleName: string) => {
	if (modules.hasOwnProperty(moduleName)) return modules[moduleName];
	throw new Error(`Dynamic require called for '${moduleName}' does not exist in core.modules!`);
});
window.require.cache = modules;
window.require.main = undefined;

// TidaLunar: published by publishReduxStore() as soon as discovery hands the store over.
export let reduxStore: Store;
let _resolveStoreReady: () => void;
export const storeReady: Promise<void> = new Promise((r) => { _resolveStoreReady = r; });

// Prefer TIDAL's real module (captured into window.__lunaHostModules by the Rust
// filter) so plugins share the host React. TIDAL's Vite output wraps React /
// ReactDOM / jsx-runtime as CJS modules: the chunk's named exports are lazy-loader
// functions (their source contains `{exports:{}` and `.exports`) and the real API
// only appears after calling one. Validate the captured object directly first,
// then invoke each lazy loader and validate its result (cf. upstream resolveCjsModule).
// Undefined when the host offers nothing valid; the caller decides what that costs.
function resolveHost<T>(id: string, valid: (m: any) => boolean): T | undefined {
	const host = (window as any).__lunaHostModules?.[id];
	if (!host) return undefined;
	if (valid(host)) {
		coreTrace.log("modules", `using host ${id}`);
		return host as T;
	}
	for (const v of Object.values(host)) {
		if (typeof v !== "function") continue;
		const src = Function.prototype.toString.call(v);
		if (!src.includes("{exports:{}") || !src.includes(".exports")) continue;
		try {
			const r = (v as () => any)();
			if (r && typeof r === "object" && valid(r)) {
				coreTrace.log("modules", `using host ${id} (cjs)`);
				return r as T;
			}
		} catch {}
	}
	return undefined;
}

// TIDAL assigns createRoot onto a CJS exports object inside its entry chunk, and that
// chunk exports nothing: no import reaches it and the capture cannot see it either. The
// Rust filter tags the assignment into globalThis.__lunaCreateRoot; graft it onto the
// captured react-dom namespace (createPortal, flushSync, ...). The module is then whole,
// every part of it bound to the host React.
function resolveHostDomClient(): any | undefined {
	// createPortal marks the react-dom namespace whether or not it still carries
	// createRoot: one lookup covers the chunk shape before and after the split.
	const dom = resolveHost<any>(
		"react-dom/client",
		(m) => typeof m.createRoot === "function" || typeof m.createPortal === "function",
	);
	if (dom === undefined) return undefined;
	if (typeof dom.createRoot === "function") return dom;
	const tagged = (globalThis as any).__lunaCreateRoot;
	if (typeof tagged !== "function") return undefined;
	coreTrace.log("modules", "completing host react-dom/client with the tagged createRoot");
	return { ...dom, createRoot: tagged };
}

/**
 * Publishes the binding every reader of the store goes through, separately from the module
 * registry below: the two become knowable at very different times. Discovery already patches
 * dispatch, so the store is in use before this returns, and withholding the binding cost the one
 * reader that cannot wait: the SDK's load delegate names the track it is loading and runs before
 * the registry exists, leaving that load's measured length unpublishable for the track's life.
 */
export function publishReduxStore(store: Store): void {
	reduxStore = store;
}

/**
 * Must be called after initTidalInternals() has populated tidalModules.
 */
export function initModules(store: Store): void {
	publishReduxStore(store);
	_resolveStoreReady();

	// React and its renderer must come from the SAME copy. react-dom writes the hook
	// dispatcher onto the internals of whichever React it closed over, while a
	// component reads the internals of the React it imported itself; a host React
	// driven by the bundled ReactDOM leaves that slot null. The first hook then throws
	// (React #321) and takes down the whole root, not just one component. The three
	// therefore resolve as a UNIT: unless the host supplies all of them, all three
	// fall back to the bundled copies, which are linked to each other at build time.
	// Mixing is never a degradation; it is the one combination that cannot render.
	const host = {
		react: resolveHost<any>(
			"react",
			(m) => typeof m.createElement === "function" && typeof m.useState === "function",
		),
		"react/jsx-runtime": resolveHost<any>("react/jsx-runtime", (m) => typeof m.jsx === "function"),
		"react-dom/client": resolveHostDomClient(),
	};
	const missing = Object.entries(host)
		.filter(([, m]) => m === undefined)
		.map(([id]) => id);

	if (missing.length === 0) {
		Object.assign(modules, host);
	} else {
		coreTrace.warn("modules", `host React incomplete (missing ${missing.join(", ")}) - using the bundled trio`);
		modules["react"] = { ...React, default: React };
		modules["react/jsx-runtime"] = { ...jsxRuntime, default: jsxRuntime };
		modules["react-dom/client"] = { ...ReactDOMClient, default: ReactDOMClient };
	}
	// The cache heal (src/ui/early_runtime/cache_heal.js) has to know whether the host
	// capture came back whole, and it cannot re-derive it: presence, validity and the CJS
	// unwrap all live here, for all three modules. It used to guess from two of them and
	// read a react-dom capture that never registered as healthy. Hand it the verdict.
	(globalThis as any).__lunaHostReady?.(missing.length === 0);
	// CJS interop: a default import (`import React from "react"`) needs `.default`.
	modules["react"].default ??= modules["react"];
	modules["react/jsx-runtime"].default ??= modules["react/jsx-runtime"];
	modules["react-dom/client"].default ??= modules["react-dom/client"];

	modules["oby"] = oby;
}
