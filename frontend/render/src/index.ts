// @luna/core - TidaLunar entry point
// Re-exports all upstream APIs. Initialization is deferred via initCore().

export { tidalModules, initTidalInternals, seedTidalConfig } from "./exposeTidalInternals";
export { buildActions, interceptors } from "./exposeTidalInternals.patchAction";

export * as ftch from "./helpers/fetch";
export { BoundedCache } from "./helpers/BoundedCache";
export { findModuleByProperty, findModuleProperty, recursiveSearch } from "./helpers/findModule";
export { unloadSet, type LunaUnload, type LunaUnloads, type NullishLunaUnloads } from "./helpers/unloadSet";

export { Messager, Tracer } from "./trace";

export { modules, defineHostModule, reduxStore, initModules } from "./modules";

export * from "./LunaPlugin";
export * from "./ReactiveStore";
export * from "./SettingsTransfer";

// Side-effect import, kept above the rest: window.core defines window.luna.
import "./window.core";

import { initTidalInternals, seedTidalConfig } from "./exposeTidalInternals";
import { initModules, publishReduxStore } from "./modules";

/**
 * Discover TIDAL internals (Redux store via React Fiber) and initialize
 * the module registry. Must be called before loading any plugins.
 */
export async function initCore(): Promise<void> {
	const { reduxStore } = await initTidalInternals();
	// Before the seed, not after it: seedTidalConfig fetches and scans every asset the page has
	// loaded and carries no timeout of its own, and the first load of a cold start lands inside
	// that gap. A load that lands there names no track, and an unnamed length is refused rather
	// than published. The track then plays to its end with no duration in the OS controls.
	publishReduxStore(reduxStore);
	await seedTidalConfig();
	initModules(reduxStore);
}
