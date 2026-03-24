import type { MaybePromise, VoidLike } from "@inrixia/helpers";
import type { ActionType } from "./types/actions/actionTypes";
import type { ActionPayloads } from "./types";

export type LunaActions = {
	[K in ActionType]: (payload: ActionPayloads[K]) => MaybePromise<VoidLike>;
};

// Lazy proxy — buildActions/reduxStore are populated by initCore() after import time.
export const actions: LunaActions = new Proxy({} as LunaActions, {
	get(_, type: string) {
		const { buildActions, reduxStore, rescanWebpackActions } = require("@luna/core");
		const isLSMI = type.includes("LOAD_SINGLE_MEDIA_ITEM");
		let buildAction = buildActions[type];
		if (buildAction) {
			if (isLSMI) console.log(`[luna:diag] actions["${type}"] → buildActions hit`);
			return (...args: any[]) => reduxStore.dispatch(buildAction(...args));
		}
		// Action creator not found — rescan webpack cache for lazy-loaded modules
		rescanWebpackActions?.();
		buildAction = buildActions[type];
		if (buildAction) {
			console.log(`[luna:actions] Rescan found: ${type}`);
			return (...args: any[]) => reduxStore.dispatch(buildAction(...args));
		}
		// Defensive: check the global observer registry in case it was populated after init
		const observerRegistry = (window as any).__LUNAR_BUILD_ACTIONS__;
		if (observerRegistry?.[type]) {
			if (isLSMI) console.log(`[luna:diag] actions["${type}"] → observer registry hit`);
			buildActions[type] = observerRegistry[type];
			return (...args: any[]) => reduxStore.dispatch(observerRegistry[type](...args));
		}
		// Fallback: dispatch raw action (for action types not discovered via webpack scan)
		if (isLSMI) console.log(`[luna:diag] actions["${type}"] → raw dispatch fallback`);
		console.warn(`[luna:actions] buildActions["${type}"] not found after rescan, using raw dispatch`);
		return (payload: any) => reduxStore?.dispatch({ type, payload });
	},
});
