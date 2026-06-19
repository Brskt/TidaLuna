import type { MaybePromise, VoidLike } from "@inrixia/helpers";
import type { ActionType } from "./types/actions/actionTypes";
import type { ActionPayloads } from "./types";

export type LunaActions = {
	[K in ActionType]: (payload: ActionPayloads[K]) => MaybePromise<VoidLike>;
};

// Under Vite/Rollup, no module cache is accessible - buildActions is empty.
// Raw dispatch ({ type, payload }) is the normal path, not a fallback.

// Types a cancel-then-redispatch interceptor re-fires (RealMAX cancels these and
// re-dispatches the same type): only these bypass interception via __lunaRawDispatch
// to break the loop. Others keep the patched dispatch so bridge interceptors still fire.
const REDISPATCH_BYPASS: ReadonlySet<string> = new Set([
	"playQueue/MOVE_TO",
	"playQueue/MOVE_NEXT",
	"playQueue/MOVE_PREVIOUS",
	"playQueue/ADD_NOW",
]);

export const actions: LunaActions = new Proxy({} as LunaActions, {
	get(_, type: string) {
		const { buildActions, reduxStore } = require("@luna/core");
		const buildAction = buildActions[type];
		if (buildAction) {
			return (...args: any[]) => reduxStore.dispatch(buildAction(...args));
		}
		return (payload: any) => {
			// Loop-prone types bypass interception; others go through the patched dispatch
			// so their bridge interceptors (e.g. playbackControls/PLAY|SEEK) still fire.
			const raw = REDISPATCH_BYPASS.has(type) ? reduxStore?.__lunaRawDispatch : undefined;
			const dispatch = raw ?? reduxStore?.dispatch?.bind(reduxStore);
			return dispatch?.({ type, payload });
		};
	},
});
