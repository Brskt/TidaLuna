import { invokeIpc, sendIpc, onIpcEvent } from "../ipc";

export function setupIpcBridge() {
    const ipcRenderer = {
        invoke: (channel: string, ...args: any[]) => {
            // Native module registration — forward to Rust which proxies to Node.js
            if (channel === "__Luna.registerNative") {
                return invokeIpc(channel, ...args);
            }
            return invokeIpc(channel, ...args);
        },
        send: (channel: string, ...args: any[]) => sendIpc(channel, ...args),
        on: (channel: string, callback: (...args: any[]) => void) =>
            onIpcEvent(channel, callback),
        once: (channel: string, callback: (...args: any[]) => void) => {
            const unsub = onIpcEvent(channel, (...args: any[]) => {
                unsub();
                callback(...args);
            });
        },
        // Stub: tidaluna:// protocol handler (not supported in CEF yet)
        onOpenUrl: (_unloads: Set<() => void>, _callback: (url: string) => void) => {},
    };

    (window as any).__ipcRenderer = ipcRenderer;
    (window as any).__platform = window.__TIDAL_RS_PLATFORM__ || "linux";
}
