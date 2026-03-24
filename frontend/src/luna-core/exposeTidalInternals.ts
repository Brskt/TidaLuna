// Runtime discovery of Redux store + action creators (replaces @uwu/quartz interception).
// 1. webpack cache (webpackChunk push trick or __webpack_require__)
// 2. React Fiber tree walk (fallback)
// 3. Proxy on webpack cache for lazy-loaded action creators

import { buildActions, interceptors } from "./exposeTidalInternals.patchAction";
import { getOrCreateLoadingContainer } from "./loadingContainer";

export const tidalModules: Record<string, object> = {};

const POLL_INTERVAL = 100;
const POLL_TIMEOUT = 30_000;

// --- Webpack cache discovery ---

function getWebpackCache(): Record<string, any> | undefined {
	const w = window as any;
	if (w.__LUNAR_WEBPACK_CACHE__) return w.__LUNAR_WEBPACK_CACHE__;

	// Strategy 1: find any global array that looks like a webpack chunk loading array
	// TIDAL may not use the standard "webpackChunk" prefix
	for (const key of Object.keys(w)) {
		const val = w[key];
		if (!Array.isArray(val)) continue;
		// webpack chunk arrays have a custom push method (webpackJsonpCallback)
		// or contain entries that are [chunkIds, moreModules, runtime] tuples
		let cache: Record<string, any> | undefined;
		try {
			val.push([
				[Symbol()],
				{},
				(require: any) => {
					if (require?.c) cache = require.c;
				},
			]);
		} catch (e) { console.warn("[luna:core] Redux store lookup failed:", e); }
		if (cache) {
			console.log(`[luna] Webpack cache found via window.${key}`);
			w.__LUNAR_WEBPACK_CACHE__ = cache;
			return cache;
		}
	}

	// Strategy 2: __webpack_require__ exposed globally
	if (typeof w.__webpack_require__?.c === "object") {
		console.log("[luna] Webpack cache found via __webpack_require__");
		w.__LUNAR_WEBPACK_CACHE__ = w.__webpack_require__.c;
		return w.__webpack_require__.c;
	}

	return undefined;
}

// --- Redux store detection ---

function isReduxStore(obj: any): boolean {
	return (
		obj != null &&
		typeof obj === "object" &&
		typeof obj.getState === "function" &&
		typeof obj.dispatch === "function" &&
		typeof obj.subscribe === "function"
	);
}

function findReduxStoreFromCache(cache: Record<string, any>): any | undefined {
	for (const id in cache) {
		const mod = cache[id];
		if (!mod?.exports) continue;
		const exports = mod.exports;
		if (typeof exports !== "object" && typeof exports !== "function") continue;
		if (isReduxStore(exports)) return exports;
		if (isReduxStore(exports.default)) return exports.default;
		if (isReduxStore(exports.store)) return exports.store;
	}
	return undefined;
}

// --- React Fiber fallback: find Redux store via DOM ---

function getFiberKey(el: Element): string | undefined {
	return Object.keys(el).find(
		(k) => k.startsWith("__reactFiber$") || k.startsWith("__reactInternalInstance$"),
	);
}

function findReactRoot(): { element: Element; fiberKey: string } | null {
	const candidates = [
		document.getElementById("wimp"),
		document.getElementById("root"),
		document.getElementById("__next"),
		document.querySelector("[data-reactroot]"),
	];

	for (const el of candidates) {
		if (!el) continue;
		const key = getFiberKey(el);
		if (key) return { element: el, fiberKey: key };
		for (const child of el.children) {
			const childKey = getFiberKey(child);
			if (childKey) return { element: child, fiberKey: childKey };
		}
	}

	if (document.body) {
		for (const child of document.body.children) {
			const key = getFiberKey(child);
			if (key) return { element: child, fiberKey: key };
			for (const grandchild of child.children) {
				const gKey = getFiberKey(grandchild);
				if (gKey) return { element: grandchild, fiberKey: gKey };
			}
		}
	}

	return null;
}

function findStoreViaReactFiber(): any | null {
	const reactRoot = findReactRoot();
	if (!reactRoot) return null;

	const queue: any[] = [(reactRoot.element as any)[reactRoot.fiberKey]];
	const seen = new WeakSet();
	while (queue.length > 0) {
		const fiber = queue.shift();
		if (!fiber || seen.has(fiber)) continue;
		seen.add(fiber);

		if (isReduxStore(fiber.memoizedProps?.store)) {
			return fiber.memoizedProps.store;
		}
		if (isReduxStore(fiber.memoizedProps?.value?.store)) {
			return fiber.memoizedProps.value.store;
		}

		if (fiber.child) queue.push(fiber.child);
		if (fiber.sibling) queue.push(fiber.sibling);
	}
	return null;
}

// --- Build actions + patch dispatch ---

function isSimpleCreator(val: any): boolean {
	return typeof val === "function" && typeof val.type === "string" && typeof val.match === "function";
}

function isThunkCreator(val: any): boolean {
	return typeof val === "function"
		&& typeof val.typePrefix === "string"
		&& typeof val.pending === "function"
		&& typeof val.fulfilled === "function"
		&& typeof val.rejected === "function";
}

function registerAction(val: any): void {
	if (isSimpleCreator(val)) {
		buildActions[val.type] = val;
	} else if (isThunkCreator(val)) {
		buildActions[val.typePrefix] = val;
	}
}

function scanModuleForActions(exports: any): void {
	if (typeof exports !== "object" && typeof exports !== "function") return;
	if (typeof exports === "function") {
		registerAction(exports);
	}
	for (const key of Object.keys(exports)) {
		try {
			registerAction(exports[key]);
		} catch (e) { /* skip */ }
	}
}

function hookWebpackCache(cache: Record<string, any>): void {
	// Wrap each cache entry's exports with a setter that scans for action creators
	// when webpack evaluates a module and writes its exports.
	//
	// webpack runtime does: cache[moduleId] = { exports: {} }; factory(module, ...);
	// The factory mutates module.exports. We intercept that by replacing the cache
	// entry object with a Proxy whose "set" trap fires on `entry.exports = ...`.
	const proxiedIds = new Set<string>();

	const cacheProxy = new Proxy(cache, {
		set(target, prop, value) {
			target[prop as string] = value;
			if (typeof prop === "string" && value && typeof value === "object" && !proxiedIds.has(prop)) {
				proxiedIds.add(prop);
				wrapModuleEntry(value);
			}
			return true;
		},
	});

	function wrapModuleEntry(entry: any): void {
		let currentExports = entry.exports;
		Object.defineProperty(entry, "exports", {
			get() { return currentExports; },
			set(val) {
				currentExports = val;
				if (val != null) {
					scanModuleForActions(val);
				}
			},
			configurable: true,
			enumerable: true,
		});
	}

	const w = window as any;
	if (w.__webpack_require__?.c === cache) {
		w.__webpack_require__.c = cacheProxy;
	}
	w.__LUNAR_WEBPACK_CACHE__ = cacheProxy;
}

export function rescanWebpackActions(): void {
	const w = window as any;
	const cache = w.__LUNAR_WEBPACK_CACHE__;
	if (!cache) return;
	for (const id in cache) {
		const mod = cache[id];
		if (mod?.exports) {
			scanModuleForActions(mod.exports);
		}
	}
}

function populateBuildActions(cache: Record<string, any> | undefined, store: any): void {
	// Scan webpack cache for RTK action creators (functions with .type and .match)
	if (cache) {
		for (const id in cache) {
			const mod = cache[id];
			if (!mod?.exports) continue;
			scanModuleForActions(mod.exports);
		}
	}

	// Patch store.dispatch to run interceptors
	const originalDispatch = store.dispatch.bind(store);
	store.dispatch = (action: any) => {
		if (action && action.type) {
			if (action.type === "content/LOAD_SINGLE_MEDIA_ITEM" || action.type === "content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS" || action.type === "content/LOAD_SINGLE_MEDIA_ITEM_FAIL") {
				console.log(`[luna:dispatch-dbg] ${action.type}`, action);
			}
			const interceptorSet = interceptors[action.type];
			if (interceptorSet?.size > 0) {
				const payload = action.payload !== undefined ? action.payload : action;
				for (const interceptor of interceptorSet) {
					try {
						const result = (interceptor as Function)(payload, action.type);
						if (result === true) return { type: "NOOP" };
						if (result instanceof Promise) result.catch((err: unknown) => console.error(`[luna:redux] Interceptor error for ${action.type}:`, err));
					} catch (e) {
						console.error(`[luna:redux] Interceptor error for ${action.type}:`, e);
					}
				}
			}
		}
		return originalDispatch(action);
	};
}

/**
 * Initialize TidaLunar internals: discover Redux store (via webpack or React Fiber)
 * and optionally populate tidalModules from webpack cache.
 * Must be called before modules.ts init.
 */
export async function initTidalInternals(): Promise<{ reduxStore: any }> {
	const { messageContainer, loadingContainer } = getOrCreateLoadingContainer();
	messageContainer.innerText = "Waiting for TIDAL to load...\n";

	const start = Date.now();
	let cache: Record<string, any> | undefined;
	let reduxStore: any;

	while (Date.now() - start < POLL_TIMEOUT) {
		// Try webpack cache first
		cache = getWebpackCache();
		if (cache) {
			reduxStore = findReduxStoreFromCache(cache);
			if (reduxStore) {
				console.log(`[luna] Redux store found via webpack cache (${Object.keys(cache).length} modules)`);
				break;
			}
		}

		// Fallback: React Fiber tree walk
		reduxStore = findStoreViaReactFiber();
		if (reduxStore) {
			// Log global arrays for debugging webpack discovery
			const arrKeys = Object.keys(window).filter((k) => Array.isArray((window as any)[k]));
			console.log("[luna] Redux store found via React Fiber (no webpack cache)");
			console.log("[luna] Global arrays on window:", arrKeys);
			for (const k of arrKeys) {
				const arr = (window as any)[k];
				console.log(`[luna]   window.${k}: length=${arr.length}, push=${arr.push?.name || typeof arr.push}, sample=`, arr[0]?.constructor?.name ?? typeof arr[0]);
			}
			const selfArrKeys = Object.keys(self).filter((k) => Array.isArray((self as any)[k]) && !arrKeys.includes(k));
			if (selfArrKeys.length > 0) {
				console.log("[luna] Additional arrays on self:", selfArrKeys);
			}
			const wpKeys =[...Object.keys(window), ...Object.keys(self)].filter(
				(k) => /webpack|chunk|modules/i.test(k)
			);
			if (wpKeys.length > 0) {
				console.log("[luna] Webpack-related globals:", wpKeys);
			}
			// Try webpack cache one more time — it may have appeared after React mounted
			cache = getWebpackCache();
			if (cache) {
				console.log(`[luna] Webpack cache found on retry (${Object.keys(cache).length} modules)`);
			}
			break;
		}

		await new Promise((r) => setTimeout(r, POLL_INTERVAL));
	}

	if (!reduxStore) {
		messageContainer.innerText = "Failed to discover TIDAL internals!";
		throw new Error("[luna] Redux store not found within timeout");
	}

	// Populate tidalModules from webpack cache exports (if available)
	if (cache) {
		for (const id in cache) {
			const mod = cache[id];
			if (mod?.exports && (typeof mod.exports === "object" || typeof mod.exports === "function")) {
				tidalModules[id] = mod.exports;
			}
		}
	}

	// Import action creators discovered by the export observer (injected before page scripts)
	const w = window as any;
	const observerRegistry = w.__LUNAR_BUILD_ACTIONS__;
	if (observerRegistry && typeof observerRegistry === "object") {
		const observerCount = Object.keys(observerRegistry).length;
		if (observerCount > 0) {
			Object.assign(buildActions, observerRegistry);
			console.log(`[luna] Imported ${observerCount} action creators from export observer`);
		}
	}
	// Rebind global to buildActions so the observer continues writing into the same object
	w.__LUNAR_BUILD_ACTIONS__ = buildActions;

	// Discover and register RTK action creators, patch dispatch for interceptors
	populateBuildActions(cache, reduxStore);

	// Hook webpack cache to discover action creators in lazy-loaded modules
	if (cache) {
		hookWebpackCache(cache);
	}

	messageContainer.innerText = "TIDAL internals discovered!";
	setTimeout(() => (loadingContainer.style.opacity = "0"), 2000);

	return { reduxStore };
}
