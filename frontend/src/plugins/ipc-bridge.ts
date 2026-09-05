import { invokeIpc, sendIpc, onIpcEvent } from "../ipc";

export function setupIpcBridge() {
    const ipcRenderer = {
        invoke: (channel: string, ...args: any[]) => {
            // This bridge is shared by the whole page and carries no plugin identity, and Rust now
            // acts on the caller's for this channel: an unattributed call is refused there. Said
            // here instead, the refusal names the route that was taken: a plugin arrives on this
            // object only when its `@luna/lib` import was not lowered per-plugin, which means a
            // hand-edited bundle or one built before that lowering existed. A generic 403 from
            // Rust would leave that indistinguishable from an expired capability.
            if (channel === "__Luna.registerNative") {
                return Promise.reject(
                    Object.assign(
                        new Error(
                            "registerNative reached the shared IPC bridge, which holds no plugin identity; rebuild the plugin with the current toolchain",
                        ),
                        { code: 403, channel },
                    ),
                );
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
    (window as any).__platform = window.__TIDALUNAR_PLATFORM__ || "linux";
}
