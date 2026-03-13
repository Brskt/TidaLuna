// this is basically electron's preload script
declare global {
    interface Window {
        cefQuery: (params: { request: string; onSuccess?: (response: string) => void; onFailure?: (errorCode: number, errorMessage: string) => void }) => void;
        nativeInterface: any;
        NativePlayerComponent: any;
        __TIDAL_CALLBACKS__: any;
        __TIDAL_IPC_RESPONSE__: any;
        __TIDAL_RS_PLATFORM__: string;
        __TIDAL_RS_WINDOW_STATE__?: { isMaximized: boolean; isFullscreen: boolean };
        __TIDAL_RS_PLAYER_PUSH__?: (events: any[]) => void;
        __TIDAL_RS_CREDENTIALS__?: {
            credentialsStorageKey: string;
            codeChallenge: string;
            redirectUri: string;
            codeVerifier: string;
        };
        __LUNAR_IPC_EMIT__?: (channel: string, ...args: any[]) => void;
        luna?: any;
        __ipcRenderer?: any;
        __platform?: string;
    }
    var window: Window & typeof globalThis;
}

export interface AudioDevice {
    controllableVolume: boolean;
    id: string;
    name: string;
}
