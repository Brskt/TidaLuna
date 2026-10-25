var _cfgTarget = { enableDesktopFeatures: true };
var _cfgProxy = new Proxy(_cfgTarget, {
    get: function(t, p) { return p === 'enableDesktopFeatures' ? true : t[p]; },
    set: function(t, p, v) { if (p !== 'enableDesktopFeatures') t[p] = v; return true; }
});
Object.defineProperty(window, 'TIDAL_CONFIG', {
    get: function() { return _cfgProxy; },
    set: function(v) {
        var src = (v && typeof v === 'object') ? v : {};
        var keys = Object.keys(src);
        for (var i = 0; i < keys.length; i++) {
            if (keys[i] !== 'enableDesktopFeatures') _cfgTarget[keys[i]] = src[keys[i]];
        }
    },
    configurable: true
});
document.title = "TidaLunar - A TIDAL client";
(function() {
    var css = '[class*="_bar_"] > [class*="_title_"] { font-size:0 !important; } [class*="_bar_"] > [class*="_title_"]::after { content:"TidaLunar - A TIDAL client"; font-size:0.75rem; } header, [role="banner"], nav[class*="bar"] { -webkit-app-region: drag; } header a, header button, header input, header [role="button"], header img, header svg, [role="banner"] a, [role="banner"] button, [role="banner"] input, [role="banner"] [role="button"], nav[class*="bar"] a, nav[class*="bar"] button, nav[class*="bar"] input, nav[class*="bar"] [role="button"] { -webkit-app-region: no-drag; }';
    function inject() {
        if (document.getElementById('tidalunar-branding')) return;
        var s = document.createElement('style');
        s.id = 'tidalunar-branding';
        s.textContent = css;
        document.head.prepend(s);
    }
    if (document.head) {
        inject();
    } else {
        document.addEventListener('DOMContentLoaded', inject);
    }
})();
