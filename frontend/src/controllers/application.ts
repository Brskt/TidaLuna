import { sendIpc, isLoginCallback } from "../ipc";

export const createApplicationController = () => {
    let delegate: any = null;


    window.__TIDAL_CALLBACKS__ = window.__TIDAL_CALLBACKS__ || {};
    window.__TIDAL_CALLBACKS__.application = {
        trigger: (method: string, ...args: any[]) => {
            if (delegate && delegate[method]) {
                delegate[method](...args);
            }
        }
    };

    return {
        applyUpdate: () => sendIpc("update.action"),
        checkForUpdatesSilently: () => sendIpc("updater.check.silently"),
        getDesktopReleaseNotes: () => JSON.stringify({}),
        getPlatform: () => window.__TIDALUNAR_PLATFORM__,
        getPlatformTarget: () => "standalone",
        // Seconds, like the official `process.uptime()`: the page's own clock runs on its differences.
        getProcessUptime: () => performance.now() / 1000,
        // Our build, never an official one: the page puts it in About and in TIDAL's error telemetry.
        getVersion: () => window.__TIDALUNAR_HOST_VERSION__ || "TidaLunar",
        getWindowsVersionNumber: () => Promise.resolve("10.0.0"),
        ready: () => {
            if (isLoginCallback()) return;
            sendIpc("web.loaded");
        },
        reenableAutoUpdater: () => sendIpc("update.reenable"),
        registerDelegate: (d: any) => { delegate = d; },
        reload: () => window.location.reload(),
        setWebVersion: (v: string) => sendIpc("web.version.set", v),
    };
};
