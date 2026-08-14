export const initWindowControls = () => {
    // Branding: force document.title to "TidaLunar - A TIDAL client".
    // Tidal's SPA overwrites the title on navigation; we use a MutationObserver
    // on the <title> element to re-apply it whenever it changes. TIDAL's own
    // titlebar component reads document.title: this also drives the visible
    // title in the in-app bar.
    const TIDALUNAR_TITLE = "TidaLunar - A TIDAL client";
    const titleEl = document.querySelector("title");
    if (titleEl) {
        // subtree is required for characterData to reach the title's child text
        // node: TIDAL (React) often updates the title by mutating that text node
        // in place rather than replacing it, which is not a childList change.
        const opts: MutationObserverInit = { childList: true, characterData: true, subtree: true };
        const titleObserver = new MutationObserver(() => {
            if (document.title !== TIDALUNAR_TITLE) {
                // Disconnect while writing, keeping our own mutation from re-firing
                // the observer, then reconnect.
                titleObserver.disconnect();
                document.title = TIDALUNAR_TITLE;
                titleObserver.observe(titleEl, opts);
            }
        });
        titleObserver.observe(titleEl, opts);
        window.addEventListener("pagehide", () => titleObserver.disconnect(), { once: true });
    }
    // Window controls + F12 live in the early runtime (fallback_titlebar.js), which
    // also works on pages without the bundle, like the login.tidal.com auth page.
};
