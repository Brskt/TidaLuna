import type { TidalStoreState } from "./types/store";
import type { Store } from "redux";

// TidaLunar: Use a lazy proxy — reduxStore is not available at import time,
// it's populated by initCore() later. This proxy defers all property access.
export const store: Store<TidalStoreState> = new Proxy({} as Store<TidalStoreState>, {
	get(_, prop) {
		// Dynamic import to avoid circular dependency at module evaluation time
		const { reduxStore } = require("@luna/core");
		if (reduxStore == null) throw new Error("[luna:lib] Redux store not initialized yet!");
		return Reflect.get(reduxStore, prop, reduxStore);
	},
});
