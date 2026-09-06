import type { LunaUnload, LunaUnloads } from "@luna/core";

/**
 * The set given here is the plugin's own, drained only when the plugin unloads. A timeout is
 * spent the moment it fires and leaves the set then: what remains otherwise is a `clearTimeout`
 * on a handle already gone, and callers that run once per row of a list fill the set with those.
 */
export const safeTimeout = (unloads: LunaUnloads, cb: () => void, delay?: number): LunaUnload => {
	const unload: LunaUnload = () => {
		clearTimeout(timeout);
		unloads?.delete(unload);
	};
	const timeout = setTimeout(() => {
		unloads?.delete(unload);
		cb();
	}, delay);
	unloads?.add(unload);
	unload.source = "safeTimeout";
	return unload;
};

/**
 * An interval is the opposite case: firing does not spend it, and its teardown is the only handle
 * anything has on a timer that is still running, and it stays registered until someone calls it.
 */
export const safeInterval = (unloads: LunaUnloads, cb: () => void, delay?: number): LunaUnload => {
	const interval = setInterval(cb, delay);
	const unload: LunaUnload = () => {
		clearInterval(interval);
		unloads?.delete(unload);
	};
	unloads?.add(unload);
	unload.source = "safeInterval";
	return unload;
};
