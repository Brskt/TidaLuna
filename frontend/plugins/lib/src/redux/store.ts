import type { TidalStoreState } from "./types/store";
import type { Store } from "redux";
import { StoreNotPublishedError } from "./storeNotPublished";

// TidaLunar: Use a lazy proxy - reduxStore is not available at import time,
// it's populated by initCore() later. This proxy defers all property access.
export const store: Store<TidalStoreState> = new Proxy({} as Store<TidalStoreState>, {
	get(_, prop) {
		// Dynamic import to avoid circular dependency at module evaluation time
		const { reduxStore } = require("@luna/core");
		if (reduxStore == null) throw new StoreNotPublishedError();
		return Reflect.get(reduxStore, prop, reduxStore);
	},
});
