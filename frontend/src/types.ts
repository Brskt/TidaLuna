// this is basically electron's preload script
declare global {
    interface Window {
        ipc: {
            postMessage: (message: string) => void;
        };
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
    }
    var window: Window & typeof globalThis;
}

export interface AudioDevice {
    controllableVolume: boolean;
    id: string;
    name: string;
}
