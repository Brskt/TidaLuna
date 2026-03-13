// --- Invoke (request/response) infrastructure ---

// --- Event push infrastructure ---

const eventListeners = new Map<string, Set<(...args: any[]) => void>>();

// Called from Rust via exec_js to push events to the frontend
(window as any).__LUNAR_IPC_EMIT__ = (channel: string, ...args: any[]) => {
    const listeners = eventListeners.get(channel);
    if (!listeners) return;
    for (const cb of listeners) {
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
    let listeners = eventListeners.get(channel);
    if (!listeners) {
        listeners = new Set();
        eventListeners.set(channel, listeners);
    }
    listeners.add(callback);
    return () => {
        listeners!.delete(callback);
    };
};
