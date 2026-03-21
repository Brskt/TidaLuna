// TidaLunar adaptation: Replace @uwu/quartz script interception with runtime discovery.
// Tidal's scripts load normally through CEF — we discover modules after the fact.
// Strategy 1: webpack cache (webpackChunk push trick or __webpack_require__)
// Strategy 2: React Fiber tree walk to find Redux Provider's store prop

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

function populateBuildActions(cache: Record<string, any> | undefined, store: any): void {
	// Scan webpack cache for RTK action creators (functions with .type and .match)
	if (cache) {
		for (const id in cache) {
			const mod = cache[id];
			if (!mod?.exports) continue;
			const exports = mod.exports;
			if (typeof exports !== "object" && typeof exports !== "function") continue;
			for (const key of Object.keys(exports)) {
				try {
					const val = exports[key];
					if (typeof val === "function" && typeof val.type === "string" && typeof val.match === "function") {
						buildActions[val.type] = val;
					}
				} catch (e) { console.warn("[luna:core] Interceptor proxy error:", e); }
			}
		}
	}

	// Patch store.dispatch to run interceptors
	const originalDispatch = store.dispatch.bind(store);
	store.dispatch = (action: any) => {
		if (action && action.type) {
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
			console.log("[luna] Redux store found via React Fiber (no webpack cache)");
			// Log global arrays for debugging webpack discovery
			const arrKeys = Object.keys(window).filter((k) => Array.isArray((window as any)[k]));
			console.log("[luna] Global arrays on window:", arrKeys);
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

	// Discover and register RTK action creators, patch dispatch for interceptors
	populateBuildActions(cache, reduxStore);

	messageContainer.innerText = "TIDAL internals discovered!";
	setTimeout(() => (loadingContainer.style.opacity = "0"), 2000);

	return { reduxStore };
}
