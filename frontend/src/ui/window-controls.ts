import { sendIpc } from "../ipc";

export const initWindowControls = () => {
    const windowState = window.__TIDAL_RS_WINDOW_STATE__ || { isMaximized: false, isFullscreen: false };

    // Frameless window management (non-Linux only).
    // On Linux, native decorations handle this (with_decorations(true)).
    const frameless = window.__TIDAL_RS_PLATFORM__ !== "linux";

    // Window controls: minimize, maximize/restore, close (Windows 10/11 style).
    if (frameless && !document.getElementById("window-controls")) {
        const svg = (paths: string) =>
            `<svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="square">${paths}</svg>`;

        const icons = {
            minimize: svg(`<line x1="1" y1="5" x2="9" y2="5"/>`),
            maximize: svg(`<rect x="1" y="1" width="8" height="8"/>`),
            restore: svg(`<polyline points="3,1 9,1 9,7"/><rect x="1" y="3" width="6" height="6"/>`),
            close: svg(`<line x1="1" y1="1" x2="9" y2="9"/><line x1="9" y1="1" x2="1" y2="9"/>`),
        };

        const style = document.getElementById("window-controls-style") || document.createElement("style");
        style.id = "window-controls-style";
        style.textContent = `
            #window-controls { position:fixed; top:0; right:0; z-index:999999; display:flex; height:32px; background:rgba(0,0,0,0.75); border-radius:0 0 0 4px; border-bottom:1px solid rgba(255,255,255,0.08); border-left:1px solid rgba(255,255,255,0.08); -webkit-app-region:no-drag; }
            #window-controls button {
                width:46px; height:32px; border:none; background:transparent;
                color:rgba(255,255,255,0.9); cursor:default; display:flex;
                align-items:center; justify-content:center; padding:0;
                outline:none; -webkit-app-region:no-drag;
            }
            #window-controls button:hover { background:rgba(255,255,255,0.1); }
            #window-controls button:active { background:rgba(255,255,255,0.05); }
            #window-controls button:focus-visible { outline:1px solid rgba(255,255,255,0.5); outline-offset:-1px; }
            #window-controls button.close:hover { background:#e81123; color:#fff; }
            #window-controls button.close:active { background:#bf0f1d; }
        `;
        document.head.appendChild(style);

        const container = document.createElement("div");
        container.id = "window-controls";

        const makeBtn = (icon: string, cls: string, label: string, onClick: () => void) => {
            const btn = document.createElement("button");
            btn.setAttribute("type", "button");
            btn.setAttribute("aria-label", label);
            btn.setAttribute("title", label);
            btn.className = cls;
            btn.innerHTML = icon;
            btn.addEventListener("click", (e) => { e.stopPropagation(); onClick(); });
            return btn;
        };

        const minimizeBtn = makeBtn(icons.minimize, "min", "Minimize", () => sendIpc("window.minimize"));
        const maximizeBtn = makeBtn(
            windowState.isMaximized ? icons.restore : icons.maximize,
            "max",
            windowState.isMaximized ? "Restore" : "Maximize",
            () => {
                const maximized = window.nativeInterface?.window?.isMaximized?.();
                sendIpc(maximized ? "window.unmaximize" : "window.maximize");
            }
        );
        const closeBtn = makeBtn(icons.close, "close", "Close", () => sendIpc("window.close"));

        container.append(minimizeBtn, maximizeBtn, closeBtn);
        document.documentElement.appendChild(container);

        // Update maximize icon and aria-label when window state changes.
        const cb = window.__TIDAL_CALLBACKS__?.window;
        if (cb) {
            cb._maxBtn = maximizeBtn;
            cb._maxIcons = icons;
            if (!cb._controlsWrapped && typeof cb.updateState === "function") {
                const origUpdateState = cb.updateState;
                cb._controlsWrapped = true;
                cb.updateState = (maximized: boolean, fullscreen: boolean) => {
                    origUpdateState(maximized, fullscreen);
                    const btn = cb._maxBtn;
                    const ic = cb._maxIcons;
                    if (btn && ic) {
                        btn.innerHTML = maximized ? ic.restore : ic.maximize;
                        btn.setAttribute("aria-label", maximized ? "Restore" : "Maximize");
                        btn.setAttribute("title", maximized ? "Restore" : "Maximize");
                    }
                };
            }
        }
    }

    // Window drag: borderless titlebar management (non-Linux only).
    // Resize hit-test/cursor/drag-resize are handled natively in Rust.
    if (frameless) {
        const INTERACTIVE = "a, button, input, select, textarea, [role='button'], img, svg";
        const FALLBACK_TITLEBAR_HEIGHT = 48;

        const getTitlebarHeight = (): number => {
            const candidates = document.querySelectorAll("header, [role='banner']");
            for (const el of candidates) {
                const rect = el.getBoundingClientRect();
                if (rect.top <= 0 && rect.height > 0 && rect.height < 120) {
                    return rect.bottom;
                }
            }
            return FALLBACK_TITLEBAR_HEIGHT;
        };

        // Mousedown: titlebar drag > passthrough.
        document.addEventListener("mousedown", (e) => {
            if (e.button !== 0) return;

            // Titlebar drag.
            if (e.clientY > getTitlebarHeight()) return;
            const target = e.target as HTMLElement | null;
            if (!target || target.closest(INTERACTIVE)) return;
            e.preventDefault();
            const maximized = window.nativeInterface?.window?.isMaximized?.();
            if (e.detail === 2) {
                sendIpc(maximized ? "window.unmaximize" : "window.maximize");
            } else {
                sendIpc("window.drag");
            }
        }, true);
    } // end frameless

    // F12 devtools fallback (in case the WebView captures the key before tao).
    document.addEventListener("keydown", (e) => {
        if (e.key === "F12") {
            e.preventDefault();
            sendIpc("window.devtools");
        }
    }, true);
};
