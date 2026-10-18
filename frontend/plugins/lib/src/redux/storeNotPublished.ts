// Kept apart from `store.ts` for a caller to recognise the error without reaching `@luna/core`.
export class StoreNotPublishedError extends Error {
	constructor() {
		super("[luna:lib] Redux store not initialized yet!");
		this.name = "StoreNotPublishedError";
	}
}

// By the name, not `instanceof`: the page holds more than one bundle, each with its own copy of the class.
export const isStoreNotPublished = (e: unknown): boolean => e instanceof Error && e.name === "StoreNotPublishedError";
