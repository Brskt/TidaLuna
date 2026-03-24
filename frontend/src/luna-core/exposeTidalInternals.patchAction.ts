// buildActions: empty under Vite/Rollup (no module cache). Kept for API compat.
// interceptors: dispatch hooks registered by index.ts for player bridge.

export const buildActions: Record<string, Function> = {};
export const interceptors: Record<string, Set<Function>> = {};
