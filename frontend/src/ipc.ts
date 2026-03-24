// Reuse the shared listener store created by early_runtime.js.

const sharedListeners: Record<string, Array<(...args: any[]) => void>> =
    (window as any).__LUNAR_IPC_LISTENERS__ ??= {};

(window as any).__LUNAR_IPC_EMIT__ = (channel: string, ...args: any[]) => {
    const cbs = sharedListeners[channel];
    if (!cbs) return;
    for (const cb of cbs) {
        try {
            cb(...args);
        } catch (e) {
            console.error("[IPC] Event handler error:", channel, e);
        }
    }
};

// --- Public API ---

export const sendIpc = (channel: string, ...args: any[]) => {
    window.cefQuery({
        request: JSON.stringify({ channel, args }),
        onSuccess: () => {},
        onFailure: (code: number, msg: string) => console.error("[IPC] FAIL:", channel, code, msg),
    });
};

let invokeCounter = 0;

export const invokeIpc = (channel: string, ...args: any[]): Promise<any> => {
    return new Promise((resolve, reject) => {
        const id = `${++invokeCounter}`;
        window.cefQuery({
            request: JSON.stringify({ channel, args, id }),
            onSuccess: (response: string) => {
                if (response.startsWith("E:")) {
                    reject(new Error(response.slice(2)));
                } else if (response.startsWith("S:")) {
                    try {
                        resolve(JSON.parse(response.slice(2)));
                    } catch (e) {
                        console.warn("[luna:ipc] Response parse error:", e);
                        resolve(response.slice(2));
                    }
                } else {
                    resolve(response);
                }
            },
            onFailure: (code: number, msg: string) => {
                reject(new Error(`[IPC] ${channel}: ${msg} (${code})`));
            },
        });
    });
};

export const onIpcEvent = (channel: string, callback: (...args: any[]) => void): (() => void) => {
    if (!sharedListeners[channel]) sharedListeners[channel] = [];
    sharedListeners[channel].push(callback);
    return () => {
        const cbs = sharedListeners[channel];
        if (cbs) {
            const idx = cbs.indexOf(callback);
            if (idx !== -1) cbs.splice(idx, 1);
        }
    };
};

(window as any).__LUNAR_IPC_ON__ = (channel: string, cb: (...args: any[]) => void) => {
    onIpcEvent(channel, cb);
};
