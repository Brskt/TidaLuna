// Resolves once TIDAL's store is published to `@luna/core`. Required on the call and not at import:
// `render/src/modules` brings the render bundle with it, which a module under test must not load.
export const storePublished = (): Promise<void> => require("../render/src/modules").storeReady;
