// Fallback window controls for frameless pages without TIDAL's `_bar_*` shell (the
// login.tidal.com auth page gets no bundle). Injected from the early runtime so it
// reaches those pages; hidden whenever TIDAL's own bar is present.
(function () {
    if (window.top !== window.self) return;
    if (window.__TL_FALLBACK_BAR__) return;
    window.__TL_FALLBACK_BAR__ = true;

    function ipc(channel) {
        if (typeof window.cefQuery === "function") {
            window.cefQuery({ request: JSON.stringify({ channel: channel, args: [] }), onSuccess: function () {}, onFailure: function () {} });
        }
    }

    // F12 -> DevTools. Lives here (not the bundle) so it also works on the login
    // page; the bundle no longer registers it, so the app never double-toggles.
    document.addEventListener("keydown", function (e) {
        if (e.key === "F12") { e.preventDefault(); ipc("window.devtools"); }
    }, true);

    function build() {
        if (!document.body || document.getElementById("tlx-titlebar")) return;

        var bar = document.createElement("div");
        bar.id = "tlx-titlebar";
        bar.style.cssText =
            "position:fixed;top:0;left:0;right:0;height:32px;display:none;" +
            "align-items:center;justify-content:space-between;z-index:2147483647;" +
            "background:rgba(0,0,0,0.85);user-select:none;-webkit-app-region:drag";

        function mkBtn(glyph, label, onClick, hoverBg) {
            var b = document.createElement("button");
            b.type = "button";
            b.textContent = glyph;
            b.title = label;
            b.style.cssText =
                "-webkit-app-region:no-drag;width:46px;height:32px;border:0;background:transparent;" +
                "color:#fff;font-size:15px;line-height:1;cursor:pointer;display:flex;" +
                "align-items:center;justify-content:center";
            b.onclick = onClick;
            b.onmouseenter = function () { b.style.background = hoverBg; };
            b.onmouseleave = function () { b.style.background = "transparent"; };
            return b;
        }

        var hover = "rgba(255,255,255,0.12)";
        var left = document.createElement("div");
        left.style.cssText = "display:flex;-webkit-app-region:no-drag";
        left.appendChild(mkBtn("‹", "Back", function () { history.back(); }, hover));

        var right = document.createElement("div");
        right.style.cssText = "display:flex;-webkit-app-region:no-drag";

        function svgEl(tag, attrs) {
            var e = document.createElementNS("http://www.w3.org/2000/svg", tag);
            for (var k in attrs) e.setAttribute(k, attrs[k]);
            return e;
        }
        function maxIcon(restore) {
            var s = svgEl("svg", { width: "11", height: "11", viewBox: "0 0 11 11", fill: "none", stroke: "currentColor" });
            if (restore) {
                s.appendChild(svgEl("rect", { x: "0.5", y: "2.5", width: "7", height: "7" }));
                s.appendChild(svgEl("path", { d: "M2.5 2.5V0.5h7v7h-2" }));
            } else {
                s.appendChild(svgEl("rect", { x: "1", y: "1", width: "9", height: "9" }));
            }
            return s;
        }
        var maxBtn = mkBtn("", "Maximize", function () { ipc("window.maximize"); }, hover);
        function setMaximized(max) {
            maxBtn.replaceChildren(maxIcon(max));
            maxBtn.title = max ? "Restore" : "Maximize";
        }
        setMaximized(!!(window.__TIDALUNAR_WINDOW_STATE__ || {}).isMaximized);
        // Rust pushes maximize/restore via __TIDAL_CALLBACKS__.window.updateState (the
        // bundle registers it on the app); define it here so the glyph stays in sync on
        // the bundle-less login page.
        window.__TIDAL_CALLBACKS__ = window.__TIDAL_CALLBACKS__ || {};
        if (!window.__TIDAL_CALLBACKS__.window) {
            window.__TIDAL_CALLBACKS__.window = { updateState: function (max) { setMaximized(!!max); } };
        }

        right.appendChild(mkBtn("−", "Minimize", function () { ipc("window.minimize"); }, hover));
        right.appendChild(maxBtn);
        right.appendChild(mkBtn("✕", "Close", function () { ipc("window.close"); }, "#e81123"));

        bar.appendChild(left);
        bar.appendChild(right);
        document.body.appendChild(bar);

        // Show only while TIDAL's bar is absent; disconnect once it mounts so the
        // app's busy DOM is not watched. `settled` defers the first show off the
        // login pages so the app never flashes the strip before `_bar_` renders.
        var cfg = self.__LUNAR_CONFIG__ || {};
        var authHosts = cfg.authHosts || [];
        var settled = authHosts.indexOf(location.hostname) !== -1 || location.pathname.indexOf("/login") === 0;

        function hasTidalBar() { return !!document.querySelector('[class*="_bar_"]'); }
        function sync() {
            if (hasTidalBar()) { bar.style.display = "none"; obs.disconnect(); return; }
            bar.style.display = settled ? "flex" : "none";
        }
        var obs = new MutationObserver(sync);
        obs.observe(document.documentElement, { childList: true, subtree: true });
        window.addEventListener("pagehide", function () { obs.disconnect(); }, { once: true });
        sync();
        if (!settled) setTimeout(function () { settled = true; sync(); }, 1500);
    }

    if (document.body) build();
    else document.addEventListener("DOMContentLoaded", build, { once: true });
})();
