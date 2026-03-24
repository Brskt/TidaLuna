// @luna/core — TidaLunar entry point
// Re-exports all upstream APIs. Initialization is deferred via initCore().

export { tidalModules, initTidalInternals } from "./exposeTidalInternals";
export { buildActions, interceptors } from "./exposeTidalInternals.patchAction";

export * as ftch from "./helpers/fetch";
export { findModuleByProperty, findModuleProperty, recursiveSearch } from "./helpers/findModule";
export { unloadSet, type LunaUnload, type LunaUnloads, type NullishLunaUnloads } from "./helpers/unloadSet";

export { Messager, Tracer } from "./trace";

export { modules, reduxStore, initModules } from "./modules";

export * from "./LunaPlugin";
export * from "./ReactiveStore";
export * from "./SettingsTransfer";

// Ensure window.luna is set up
import "./window.core";

import { initTidalInternals } from "./exposeTidalInternals";
import { initModules } from "./modules";

/**
 * Discover TIDAL internals (Redux store via React Fiber) and initialize
 * the module registry. Must be called before loading any plugins.
 */
export async function initCore(): Promise<void> {
	const { reduxStore } = await initTidalInternals();
	initModules(reduxStore);
}
