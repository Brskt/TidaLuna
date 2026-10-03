import type { ActionPayloads } from "./types";
import type { ActionType } from "./types/actions/actionTypes";

export type InterceptPayload<T extends ActionType | ReadonlyArray<ActionType>> =
	T extends ReadonlyArray<infer U extends ActionType> // Check if T is an array, ensure U is ActionType
		? InterceptPayload<U> // If T is ActionType[], give a union of all payloads
		: T extends keyof ActionPayloads
			? ActionPayloads[T]
			: never;

export type InterceptCallback<T extends ActionType | ActionType[], P = InterceptPayload<T>> = (payload: P, type: ActionType) => true | unknown;
export type LunaInterceptors = {
	[K in ActionType]?: Set<InterceptCallback<K>>;
};

import { interceptors, putInterceptors, type LunaUnload, type LunaUnloads } from "@luna/core";

export type InterceptOptions = {
	once?: boolean;
	/**
	 * Also see the action when a TIDAL saga `put`s it, which never goes through the hooked `dispatch`.
	 * Off by default: TIDAL both puts and dispatches some types (PLAY, SEEK, SET_VOLUME), and this
	 * would see each of those twice.
	 */
	sagaPuts?: boolean;
};

/**
 * Intercept a Redux action based on its `type`
 * @param actionType The ActionKey to intercept
 * @param cb Called when action is intercepted with action args, if returning true action is not dispatched (cancelled)
 * @param unloads Set of unload functions to add this to, can be null but only pass if you know what your doing
 * @param options `true` for once, or an {@link InterceptOptions}
 * @returns Function to call to unload/cancel the intercept
 */
export function intercept<T extends ActionType>(actionType: T, unloads: LunaUnloads, cb: InterceptCallback<T>, options?: boolean | InterceptOptions): LunaUnload;
export function intercept<T extends ActionType[]>(actionType: T, unloads: LunaUnloads, cb: InterceptCallback<T>, options?: boolean | InterceptOptions): LunaUnload;
export function intercept<T extends ActionType | ActionType[]>(
	actionTypes: T,
	unloads: LunaUnloads,
	cb: InterceptCallback<T>,
	options?: boolean | InterceptOptions,
): LunaUnload {
	const actionTypeArray: ActionType[] = Array.isArray(actionTypes) ? actionTypes : [actionTypes];
	const once = typeof options === "boolean" ? options : options?.once;
	const registries = typeof options === "object" && options.sagaPuts ? [interceptors, putInterceptors] : [interceptors];

	// If once is true then call unIntercept immediately to only run once
	const intercept = once
		? (payload: InterceptPayload<T>, type: ActionType) => {
				unIntercept();
				return cb(payload, type);
			}
		: cb;

	// Wrap removing the callback from the interceptors in a unload function and return it
	const unIntercept = () => {
		for (const registry of registries) {
			for (const actionType of actionTypeArray) {
				// ?. so that it doesn't throw if the interceptor was already removed
				registry[actionType]?.delete(intercept);
				if (registry[actionType]?.size === 0) delete registry[actionType];
			}
		}
		// And leaves the caller's set, the way `registerEmitter`'s own unload does: an intercept
		// already removed has nothing left to undo, and that set is the plugin's own, drained
		// once, at teardown.
		unloads?.delete(unIntercept);
	};
	unIntercept.source = `intercept${JSON.stringify(actionTypeArray)}`;

	for (const registry of registries) {
		for (const actionType of actionTypeArray) {
			registry[actionType] ??= new Set<InterceptCallback<T>>();
			registry[actionType].add(intercept);
		}
	}

	unloads?.add(unIntercept);
	return unIntercept;
}

/**
 * Intercept a Redux action based on its `type`
 * @param actionType The ActionKey to intercept
 * @param unloads Set of unload functions to add this to, can be null but only pass if you know what your doing
 * @param cancel If set true the action is not dispatched (cancelled)
 * @returns A promise that resolves with the action payload when the action is intercepted
 */
export function interceptPromise<T extends ActionType>(actionType: T, unloads: LunaUnloads, cancel?: true): Promise<InterceptPayload<T>>;
export function interceptPromise<T extends ActionType>(actionType: T, unloads: LunaUnloads, cancel?: true): Promise<InterceptPayload<T>> {
	const { resolve, promise } = Promise.withResolvers<InterceptPayload<T>>();
	intercept(
		actionType,
		unloads,
		(payload) => {
			resolve(payload);
			return cancel;
		},
		true,
	);
	return promise;
}
