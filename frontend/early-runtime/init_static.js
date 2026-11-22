// Runs in the early-runtime payload, before TIDAL's inline script and modules: `index` reads
// `enableDesktopFeatures` once while it evaluates, and a late value turns the page into the web player.
(function() {
    var cfgTarget = { enableDesktopFeatures: true };
    var cfgProxy = new Proxy(cfgTarget, {
        get: function(t, p) { return p === 'enableDesktopFeatures' ? true : t[p]; },
        set: function(t, p, v) { if (p !== 'enableDesktopFeatures') t[p] = v; return true; }
    });
    Object.defineProperty(window, 'TIDAL_CONFIG', {
        get: function() { return cfgProxy; },
        set: function(v) {
            var src = (v && typeof v === 'object') ? v : {};
            var keys = Object.keys(src);
            for (var i = 0; i < keys.length; i++) {
                if (keys[i] !== 'enableDesktopFeatures') cfgTarget[keys[i]] = src[keys[i]];
            }
        },
        configurable: true
    });

    // The document has no <head> yet, and a title written now is dropped: both wait for the head.
    // TIDAL's page sets no title of its own; this is the only one the window gets.
    var title = "TidaLunar - A TIDAL client";
    var css = '[class*="_bar_"] > [class*="_title_"] { font-size:0 !important; } [class*="_bar_"] > [class*="_title_"]::after { content:"TidaLunar - A TIDAL client"; font-size:0.75rem; } header, [role="banner"], nav[class*="bar"] { -webkit-app-region: drag; } header a, header button, header input, header [role="button"], header img, header svg, [role="banner"] a, [role="banner"] button, [role="banner"] input, [role="banner"] [role="button"], nav[class*="bar"] a, nav[class*="bar"] button, nav[class*="bar"] input, nav[class*="bar"] [role="button"] { -webkit-app-region: no-drag; }';
    function brand() {
        if (!document.head) return false;
        document.title = title;
        if (!document.getElementById('tidalunar-branding')) {
            var s = document.createElement('style');
            s.id = 'tidalunar-branding';
            s.textContent = css;
            document.head.prepend(s);
        }
        return true;
    }
    if (brand()) return;
    var observer = new MutationObserver(function() {
        if (brand()) observer.disconnect();
    });
    observer.observe(document, { childList: true, subtree: true });
    document.addEventListener('DOMContentLoaded', function() {
        observer.disconnect();
        brand();
    });
})();
