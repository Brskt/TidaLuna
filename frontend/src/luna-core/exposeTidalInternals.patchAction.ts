// TidaLunar adaptation: buildActions is populated by exposeTidalInternals.initTidalInternals()
// instead of quartz-transformed patchAction calls.

export const buildActions: Record<string, Function> = {};
export const interceptors: Record<string, Set<Function>> = {};
