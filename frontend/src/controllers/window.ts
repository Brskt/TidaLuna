import { sendIpc } from "../ipc";

export const createWindowController = (initialState: { isMaximized: boolean, isFullscreen: boolean }) => {
    let isMaximized = initialState.isMaximized;
    let isFullscreen = initialState.isFullscreen;
    const listeners: Record<string, Function[]> = {};

    const updateState = (maximized: boolean, fullscreen: boolean) => {
        const maxChanged = isMaximized !== maximized;
        const fullChanged = isFullscreen !== fullscreen;

        isMaximized = maximized;
        isFullscreen = fullscreen;

        if (maxChanged) {
            const event = maximized ? "maximize" : "unmaximize";
            if (listeners[event]) listeners[event].forEach(cb => cb());
        }
        if (fullChanged) {
            const event = fullscreen ? "enter-full-screen" : "leave-full-screen";
            if (listeners[event]) listeners[event].forEach(cb => cb());
        }
    };

    window.__TIDAL_CALLBACKS__ = window.__TIDAL_CALLBACKS__ || {};
    window.__TIDAL_CALLBACKS__.window = {
        updateState: updateState
    };

    return {
        close: () => sendIpc("window.close"),
        isFullscreen: () => isFullscreen,
        isMaximized: () => isMaximized,
        maximize: () => sendIpc("window.maximize"),
        minimize: () => sendIpc("window.minimize"),
        on: (event: string, callback: any) => {
            if (!listeners[event]) listeners[event] = [];
            listeners[event].push(callback);
        },
        openMenu: (x: number, y: number) => sendIpc("menu.clicked", x, y),
        unmaximize: () => sendIpc("window.unmaximize"),
    }
}
