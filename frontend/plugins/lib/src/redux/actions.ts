import type { MaybePromise, VoidLike } from "@inrixia/helpers";
import type { ActionType } from "./types/actions/actionTypes";
import type { ActionPayloads } from "./types";

export type LunaActions = {
	[K in ActionType]: (payload: ActionPayloads[K]) => MaybePromise<VoidLike>;
};

// Under Vite/Rollup, no module cache is accessible - buildActions is empty.
// Raw dispatch ({ type, payload }) is the normal path, not a fallback.
export const actions: LunaActions = new Proxy({} as LunaActions, {
	get(_, type: string) {
		const { buildActions, reduxStore } = require("@luna/core");
		const buildAction = buildActions[type];
		if (buildAction) {
			return (...args: any[]) => reduxStore.dispatch(buildAction(...args));
		}
		return (payload: any) => reduxStore?.dispatch({ type, payload });
	},
});
