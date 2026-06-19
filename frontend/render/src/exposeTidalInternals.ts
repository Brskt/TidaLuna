// Runtime discovery of Redux store via React Fiber tree walk.
// TIDAL uses Vite/Rollup - no webpack runtime, no module cache accessible.

import { interceptors } from "./exposeTidalInternals.patchAction";

// Best-effort module registry. Empty under Vite/Rollup - no module cache accessible.
export const tidalModules: Record<string, object> = {};

const POLL_INTERVAL = 100;
const POLL_TIMEOUT = 30_000;

// --- Redux store detection via React Fiber ---

function isReduxStore(obj: any): boolean {
	return (
		obj != null &&
		typeof obj === "object" &&
		typeof obj.getState === "function" &&
		typeof obj.dispatch === "function" &&
		typeof obj.subscribe === "function"
	);
}

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

// --- Dispatch interceptors ---

// Queue navigation must never be frozen by an interceptor's cancel. A plugin like
// RealMAX cancels MOVE_* to run an async max-quality lookup, which blocks the click
// until the network round-trip finishes. For these types we still RUN the interceptor
// (its async re-dispatch applies the quality swap afterward) but ignore the cancel, so
// the original move proceeds immediately and the track plays at once. ADD_NOW is left
// cancellable: it mutates the queue, so letting both the original and the re-dispatch
// through would double-add.
const NON_BLOCKING_NAV: ReadonlySet<string> = new Set([
	"playQueue/MOVE_TO",
	"playQueue/MOVE_NEXT",
	"playQueue/MOVE_PREVIOUS",
]);

function patchDispatch(store: any): void {
	const originalDispatch = store.dispatch.bind(store);
	// Unintercepted re-dispatch channel for redux.actions[type], so a plugin's
	// cancel-then-redispatch can't re-enter and loop; mirrors upstream's unwrapped creator.
	store.__lunaRawDispatch = originalDispatch;
	store.dispatch = (action: any) => {
		if (action && action.type) {
			const interceptorSet = interceptors[action.type];
			if (interceptorSet?.size > 0) {
				const payload = action.payload !== undefined ? action.payload : action;
				for (const interceptor of interceptorSet) {
					try {
						const result = (interceptor as Function)(payload, action.type);
						// A cancel (true) drops the action, except queue nav, which stays
						// instant (the interceptor re-dispatches the quality swap afterward).
						if (result === true && !NON_BLOCKING_NAV.has(action.type)) {
							return { type: "NOOP" };
						}
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

// TIDAL config constants (API keys, URLs, feature values) are scope-hoisted bundle
// literals that plugins read by name via findModuleProperty. Vite leaves them
// unreachable as live objects, so harvest every backtick-string object property from
// the loaded chunk text into tidalModules before plugins load.
export async function seedTidalConfig(): Promise<void> {
	const urls = new Set<string>();
	for (const s of document.scripts) if (s.src) urls.add(s.src);
	try {
		for (const r of performance.getEntriesByType("resource")) if (/\/assets\/[^?]*\.m?js(\?|$)/i.test(r.name)) urls.add(r.name);
	} catch {}

	const constants: Record<string, string> = {};
	await Promise.all(
		[...urls].map(async (url) => {
			try {
				if (new URL(url, location.href).origin !== location.origin) return;
				const text = await (await fetch(url)).text();
				// `ident: `value`` object props with a plain (non-interpolated, non-escaped) value.
				const re = /["']?([A-Za-z_$][\w$]*)["']?\s*:\s*`([^`$\\]{1,4096})`/g;
				for (const [, key, value] of text.matchAll(re)) constants[key] = value;
			} catch {}
		}),
	);

	const count = Object.keys(constants).length;
	if (count > 0) {
		tidalModules.tidalBundleConstants = constants;
		console.log(`[luna] Seeded ${count} TIDAL bundle constants`);
	} else {
		console.warn("[luna] No TIDAL bundle constants found - findModuleProperty lookups will miss");
	}
}

export async function initTidalInternals(): Promise<{ reduxStore: any }> {
	const start = Date.now();
	let reduxStore: any;

	while (Date.now() - start < POLL_TIMEOUT) {
		reduxStore = findStoreViaReactFiber();
		if (reduxStore) {
			console.log("[luna] Redux store found via React Fiber");
			break;
		}
		await new Promise((r) => setTimeout(r, POLL_INTERVAL));
	}

	if (!reduxStore) {
		document.title = "TidaLunar - Failed to initialize";
		throw new Error("[luna] Redux store not found within timeout");
	}

	patchDispatch(reduxStore);
	exposeStateSlicesToModules(reduxStore);
	publishLunarPlayer();

	return { reduxStore };
}

// Expose top-level Redux state slices on tidalModules via live getters so
// findModuleProperty / findModuleByProperty can resolve runtime state (e.g.
// activePlayer.currentTime), not just the bundle string constants seedTidalConfig
// harvests. State is immutable, so each getter pulls the current snapshot.
function exposeStateSlicesToModules(reduxStore: any): void {
	for (const slice of Object.keys(reduxStore.getState())) {
		Object.defineProperty(tidalModules, slice, {
			configurable: true,
			enumerable: true,
			get: () => reduxStore.getState()[slice],
		});
	}
}

// Satisfy @luna/lib PlayState.currentTime selector (TIDAL's player SDK singleton has no analog here).
function publishLunarPlayer(): void {
	tidalModules.__lunarPlayer = { activePlayer: (window as any).NativePlayerComponent.activePlayer };
}
