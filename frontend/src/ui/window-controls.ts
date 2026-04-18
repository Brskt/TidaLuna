import { sendIpc } from "../ipc";

export const initWindowControls = () => {
    // Branding: force document.title to "TidaLunar - A TIDAL client".
    // Tidal's SPA overwrites the title on navigation, so we use a MutationObserver
    // on the <title> element to re-apply it whenever it changes. TIDAL's own
    // titlebar component reads document.title, so this also drives the visible
    // title in the in-app bar.
    const TIDALUNAR_TITLE = "TidaLunar - A TIDAL client";
    const titleEl = document.querySelector("title");
    if (titleEl) {
        new MutationObserver(() => {
            if (document.title !== TIDALUNAR_TITLE) {
                document.title = TIDALUNAR_TITLE;
            }
        }).observe(titleEl, { childList: true, characterData: true, subtree: true });
    }

    // F12 devtools fallback (in case CEF captures the key before the page).
    document.addEventListener("keydown", (e) => {
        if (e.key === "F12") {
            e.preventDefault();
            sendIpc("window.devtools");
        }
    }, true);
};
