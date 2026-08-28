// Self-heal for a Service Worker cache that predates one of the Rust rewrites. Those
// filters only rewrite what crosses the network. A chunk already sitting in Cache Storage
// keeps the shape it had when it was precached: a module capture or a createRoot tag
// added afterwards is invisible to it. Dropping the worker and its caches makes the next
// launch refetch everything through the filters.
//
// Injected as part of `init_script` (see app_bootstrap.rs): runs on every real
// navigation, before the bundle.
(function () {
    // Announced, never silent: this drops every cache the origin has, and its effect only
    // shows on the NEXT launch; a run that busts and a run that recovers are two different
    // logs. The reason travels with the call to keep one line for both callers.
    function bust(reason) {
        try {
            if (!('serviceWorker' in navigator) || !navigator.serviceWorker.controller || !window.caches) return;
            console.warn('[luna:heal] ' + reason + ' - dropping the service worker and its caches, the next launch refetches them');
            navigator.serviceWorker.getRegistrations().then(function (rs) { rs.forEach(function (r) { r.unregister(); }); });
            caches.keys().then(function (ks) { ks.forEach(function (k) { caches.delete(k); }); });
        } catch (e) {}
    }

    // 1) Stale CSP meta on a SW-served shell -> re-precache the stripped shell.
    function cspHeal() {
        if (document.querySelector('meta[http-equiv="Content-Security-Policy" i]')) bust('stale CSP meta on a cached shell');
    }

    // 2) The host React capture came back incomplete; the renderer fell back to its
    //    bundled trio. Whether that happened is NOT decided here: `initModules` owns the
    //    verdict, because only it knows presence, validity and the CJS unwrap, for all
    //    three modules. A second criterion used to live here and disagreed with it. It
    //    read a react-dom capture that never registered as healthy, and never healed it.
    //    The bundle hands the verdict over instead, through `__lunaHostReady`.
    var verdictSeen = false;

    function heal(ready) {
        try {
            if (sessionStorage.getItem('__luna_react_heal')) return;
            // A whole capture leaves nothing to heal and nothing to remember. This comes
            // BEFORE the controller test: the session right after a bust runs uncontrolled
            // (the re-registered worker never claims the page that outlived it), and that
            // is exactly the session that proves the heal worked.
            if (ready) { localStorage.removeItem('__luna_heal_spent'); return; }
            // Only a controlling worker can be serving a stale cache. Without one there is
            // nothing a bust could repair.
            if (!navigator.serviceWorker || !navigator.serviceWorker.controller) return;
            // One bust is all a stale cache needs: a later session that still misses means
            // a pattern stopped matching rather than the cache being stale. Stop there
            // instead of re-precaching on every launch. Log it, because this is the only
            // warning that TIDAL's bundle moved out from under a rewrite.
            if (localStorage.getItem('__luna_heal_spent')) {
                console.warn('[luna:heal] host React capture still incomplete after a bust: a rewrite no longer matches TIDAL\'s bundle, bundled React stands in');
                return;
            }
            localStorage.setItem('__luna_heal_spent', '1');
            sessionStorage.setItem('__luna_react_heal', '1');
            bust('host React capture incomplete');
        } catch (e) {}
    }

    globalThis.__lunaHostReady = function (ok) { verdictSeen = true; heal(!!ok); };

    // No verdict means the bundle never got as far as resolving the modules: its store
    // discovery gives up only after 30 s, and its config seeding has no timeout at all;
    // a wedged worker (the very fault this heals) can stall it indefinitely. Nothing
    // authoritative exists then. Fall back to the raw capture side effects, which land
    // as TIDAL's own chunks execute and do not depend on the bundle at all. Coarser than
    // the verdict, and only ever consulted when there is no verdict to prefer.
    function fallback() {
        if (verdictSeen) return;
        console.warn('[luna:heal] no readiness verdict after 35s - the bundle never finished its own init, judging on the raw capture instead');
        heal(!!(window.__lunaHostModules && window.__lunaHostModules.react)
            && typeof globalThis.__lunaCreateRoot === 'function');
    }

    function run() { cspHeal(); setTimeout(fallback, 35000); }
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', run);
    else run();
})();
