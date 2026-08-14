// Sink for TIDAL's real module namespaces: the Rust capture filter appends
// __LUNA_CAP(id, ns) to React-family chunks, letting plugins resolve the host React.
window.__lunaHostModules = window.__lunaHostModules || {};
// First writer wins: the real react chunk is modulepreloaded and runs before any
// lazy vendor chunk; a later same-id capture cannot clobber a valid one.
globalThis.__LUNA_CAP = function (id, ns) { if (!window.__lunaHostModules[id]) window.__lunaHostModules[id] = ns; };
