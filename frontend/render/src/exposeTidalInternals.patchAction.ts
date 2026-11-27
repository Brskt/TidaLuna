// buildActions: empty under Vite/Rollup (no module cache). Kept for API compat.
// interceptors: dispatch hooks registered by index.ts for player bridge.
// putInterceptors: hooks on the actions a saga `put`s, opted into one by one; see
// `installSagaPutTap` for why they are not the same registry.

export const buildActions: Record<string, Function> = {};
export const interceptors: Record<string, Set<Function>> = {};
export const putInterceptors: Record<string, Set<Function>> = {};

// Queue navigation ignores an interceptor's cancel: RealMAX cancels MOVE_* for an async quality
// lookup that would freeze the click. The interceptor still RUNS (its re-dispatch swaps the quality
// afterward). ADD_NOW stays cancellable, or the original and the re-dispatch would both add.
const NON_BLOCKING_NAV: ReadonlySet<string> = new Set([
	"playQueue/MOVE_TO",
	"playQueue/MOVE_NEXT",
	"playQueue/MOVE_PREVIOUS",
]);

// Run one registry's interceptors over an action; true when one cancelled it. Shared by the
// dispatch rail, the saga-put tap and `deliver`: a cancel means the same on all three.
export function runInterceptors(interceptorSet: Set<Function> | undefined, action: any): boolean {
	if (!(interceptorSet?.size > 0)) return false;
	const payload = action.payload !== undefined ? action.payload : action;
	for (const interceptor of interceptorSet) {
		try {
			const result = interceptor(payload, action.type);
			// A cancel (true) drops the action, except queue nav, which stays
			// instant (the interceptor re-dispatches the quality swap afterward).
			if (result === true && !NON_BLOCKING_NAV.has(action.type)) return true;
			if (result instanceof Promise) result.catch((err: unknown) => console.error(`[luna:redux] Interceptor error for ${action.type}:`, err));
		} catch (e) {
			console.error(`[luna:redux] Interceptor error for ${action.type}:`, e);
		}
	}
	return false;
}

// An action for Luna's dispatch interceptors only, kept out of TIDAL's store: what the store
// already received from TIDAL itself must not be counted twice by its reducers, while plugins,
// which never see a saga's put, still learn of it. True when an interceptor cancelled it.
export function deliver(action: { type: string; payload?: unknown }): boolean {
	return runInterceptors(interceptors[action.type], action);
}
