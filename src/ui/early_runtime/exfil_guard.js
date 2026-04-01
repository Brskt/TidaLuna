// Fragment 6/6 — Exfiltration guard: lock sendBeacon + Image src
// Defence-in-depth: CEF-level blocking is the structural guarantee.
// These JS locks raise the bar for plugins that escape the IIFE wrapper.

// --- sendBeacon: TIDAL doesn't use it, block entirely ---
Object.defineProperty(navigator, 'sendBeacon', {
    get: function() { return function() { return false; }; },
    set: function() {},
    enumerable: true,
    configurable: false
});

// --- HTMLImageElement.prototype.src: allowlist (tidal.com, gravatar, github, data/blob) ---
var _origSrcDesc = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
if (_origSrcDesc && _origSrcDesc.set) {
    Object.defineProperty(HTMLImageElement.prototype, 'src', {
        get: _origSrcDesc.get,
        set: function(url) {
            if (typeof url === 'string' && url.indexOf('://') !== -1
                && url.indexOf('tidal.com') === -1
                && url.indexOf('gravatar.com') === -1
                && url.indexOf('github.com') === -1
                && url.indexOf('githubusercontent.com') === -1
                && url.indexOf('data:') !== 0
                && url.indexOf('blob:') !== 0) {
                return;
            }
            _origSrcDesc.set.call(this, url);
        },
        enumerable: true,
        configurable: false
    });
}
