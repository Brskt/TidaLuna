import type { Store } from "redux";
import { coreTrace } from "./trace/Tracer";

// Bundled fallbacks - used when the host module wasn't captured (a 2nd React copy)
import * as React from "react";
import * as ReactDOMClient from "react-dom/client";
import * as jsxRuntime from "react/jsx-runtime";
import * as oby from "oby";

export const modules: Record<string, any> = {};

// Define a global require function to use modules for cjs imports bundled with esbuild
window.require = <NodeJS.Require>((moduleName: string) => {
	if (modules.hasOwnProperty(moduleName)) return modules[moduleName];
	throw new Error(`Dynamic require called for '${moduleName}' does not exist in core.modules!`);
});
window.require.cache = modules;
window.require.main = undefined;

// TidaLunar: reduxStore is assigned by initModules() after webpack/Redux discovery.
export let reduxStore: Store;
let _resolveStoreReady: () => void;
export const storeReady: Promise<void> = new Promise((r) => { _resolveStoreReady = r; });

// Prefer TIDAL's real module (captured into window.__lunaHostModules by the Rust
// filter) so plugins share the host React. TIDAL's Vite output wraps React /
// ReactDOM / jsx-runtime as CJS modules: the chunk's named exports are lazy-loader
// functions (their source contains `{exports:{}` and `.exports`) and the real API
// only appears after calling one. So validate the captured object directly first,
// then invoke each lazy loader and validate its result (cf. upstream resolveCjsModule).
// Bundled copy (a second React instance) only if neither yields a valid module.
function resolveModule<T>(id: string, valid: (m: any) => boolean, bundled: T): T {
	const host = (window as any).__lunaHostModules?.[id];
	if (host) {
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
	}
	coreTrace.warn("modules", `host ${id} unavailable - using bundled`);
	return bundled;
}

/**
 * Must be called after initTidalInternals() has populated tidalModules.
 */
export function initModules(store: Store): void {
	reduxStore = store;
	_resolveStoreReady();

	modules["react"] = resolveModule(
		"react",
		(m) => typeof m.createElement === "function" && typeof m.useState === "function",
		{ ...React, default: React },
	);
	modules["react/jsx-runtime"] = resolveModule(
		"react/jsx-runtime",
		(m) => typeof m.jsx === "function",
		{ ...jsxRuntime, default: jsxRuntime },
	);
	modules["react-dom/client"] = resolveModule(
		"react-dom/client",
		(m) => typeof m.createRoot === "function",
		{ ...ReactDOMClient, default: ReactDOMClient },
	);
	// CJS interop: a default import (`import React from "react"`) needs `.default`.
	modules["react"].default ??= modules["react"];
	modules["react/jsx-runtime"].default ??= modules["react/jsx-runtime"];
	modules["react-dom/client"].default ??= modules["react-dom/client"];

	modules["oby"] = oby;
}
