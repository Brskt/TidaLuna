import type { MaybePromise, VoidLike } from "@inrixia/helpers";
import type { ActionType } from "./types/actions/actionTypes";
import type { ActionPayloads } from "./types";

export type LunaActions = {
	[K in ActionType]: (payload: ActionPayloads[K]) => MaybePromise<VoidLike>;
};

// TidaLunar: Use a lazy proxy — buildActions and reduxStore are not available
// at import time, they're populated by initCore() later.
// The proxy creates action dispatchers on-demand when accessed.
export const actions: LunaActions = new Proxy({} as LunaActions, {
	get(_, type: string) {
		const { buildActions, reduxStore } = require("@luna/core");
		const buildAction = buildActions[type];
		if (buildAction) return (...args: any[]) => reduxStore.dispatch(buildAction(...args));
		// Fallback: dispatch raw action (for action types not discovered via webpack scan)
		return (payload: any) => reduxStore?.dispatch({ type, payload });
	},
});
