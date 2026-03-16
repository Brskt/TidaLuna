// QuickJS override: module resolution handled by rquickjs LunaLoader.
// No window.require needed — plugins use ES module imports.

export const modules: Record<string, any> = {};

export let reduxStore: any;

export function initModules(store: any): void {
	reduxStore = store;
}
