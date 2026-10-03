// buildActions: empty under Vite/Rollup (no module cache). Kept for API compat.
// interceptors: dispatch hooks registered by index.ts for player bridge.
// putInterceptors: hooks on the actions a saga `put`s, opted into one by one; see
// `installSagaPutTap` for why they are not the same registry.

export const buildActions: Record<string, Function> = {};
export const interceptors: Record<string, Set<Function>> = {};
export const putInterceptors: Record<string, Set<Function>> = {};
