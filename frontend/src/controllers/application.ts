import { sendIpc } from "../ipc";

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
        getPlatform: () => window.__TIDAL_RS_PLATFORM__,
        getPlatformTarget: () => "standalone",
        getProcessUptime: () => 100,
        getVersion: () => "2.38.6.6",
        getWindowsVersionNumber: () => Promise.resolve("10.0.0"),
        ready: () => sendIpc("web.loaded"),
        reenableAutoUpdater: () => sendIpc("update.reenable"),
        registerDelegate: (d: any) => { delegate = d; },
        reload: () => window.location.reload(),
        setWebVersion: (v: string) => sendIpc("web.version.set", v),
    };
};
