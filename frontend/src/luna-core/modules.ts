import type { Store } from "redux";
import { findModuleByProperty } from "./helpers/findModule";

// Bundled fallbacks — used when TIDAL's webpack cache isn't accessible
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

/**
 * Must be called after initTidalInternals() has populated tidalModules.
 */
export function initModules(store: Store): void {
	reduxStore = store;

	// Expose react — try TIDAL's webpack cache first, fall back to bundled packages
	modules["react"] = findModuleByProperty((key, value) => key === "createElement" && typeof value === "function");
	if (!modules["react"]) {
		const reactMod = { ...React, default: React };
		modules["react"] = reactMod;
	} else {
		modules["react"].default ??= modules["react"];
	}

	modules["react/jsx-runtime"] = findModuleByProperty((key, value) => key === "jsx" && typeof value === "function");
	if (!modules["react/jsx-runtime"]) {
		const jsxMod = { ...jsxRuntime, default: jsxRuntime };
		modules["react/jsx-runtime"] = jsxMod;
	} else {
		modules["react/jsx-runtime"].default ??= modules["react/jsx-runtime"];
	}

	modules["react-dom/client"] = findModuleByProperty((key, value) => key === "createRoot" && typeof value === "function");
	if (!modules["react-dom/client"]) {
		modules["react-dom/client"] = ReactDOMClient;
	} else {
		modules["react-dom/client"].default ??= modules["react-dom/client"];
	}

	// oby is always bundled with us
	modules["oby"] = oby;
}
