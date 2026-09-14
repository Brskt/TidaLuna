// Fragment 6/6 - Exfiltration guard: lock sendBeacon
// Defence-in-depth: ExfilBlockHandler cancels an RT_PING bound for a non-Tidal origin and is
// the structural guarantee; this lock only raises the bar for plugins escaping the IIFE wrapper.
//
// Image loads belong to nav::is_allowed_image_host, not here: a DOM accessor sees the `src`
// property alone, which setAttribute, srcset, a <picture> pick, a CSS background and React's
// commit path all walk past, and an iframe carrying no early runtime hands back a pristine
// HTMLImageElement anyway. A copy here would only give the host list a second place to drift.

// --- sendBeacon: TIDAL doesn't use it, block entirely ---
Object.defineProperty(navigator, 'sendBeacon', {
    get: function() { return function() { return false; }; },
    set: function() {},
    enumerable: true,
    configurable: false
});
