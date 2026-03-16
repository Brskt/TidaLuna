// QuickJS override: no webpack cache, no React Fiber, no DOM.
// tidalModules stays empty — module discovery happens in CEF via bridge.

export const tidalModules: Record<string, object> = {};

export async function initTidalInternals(): Promise<{ reduxStore: any }> {
	// In QuickJS, the Redux store is provided via the CEF bridge (__cef_state).
	// Return a minimal stub store — plugins that need dispatch/getState
	// will go through the bridge's __intercept / __cef_eval.
	const stubStore = {
		getState: () => ({}),
		dispatch: (_action: any) => {},
		subscribe: (_listener: any) => () => {},
	};
	return { reduxStore: stubStore };
}
