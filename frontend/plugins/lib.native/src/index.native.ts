/**
 * Stub for @luna/lib.native - functions that require Electron/Node.js in TidaLuna.
 * In TidaLunar (CEF), these are either no-ops or adapted to Rust IPC.
 */

export const pkg = async () => ({
    version: __TIDALUNAR_VERSION__,
    name: "TidaLunar",
});

export const relaunch = async () => {
    window.location.reload();
};

export const update = async (_version: string): Promise<string | void> => {
    console.warn("[luna:native] update() is not supported in TidaLunar");
};

export const needsElevation = async (): Promise<boolean> => false;

export const runElevatedInstall = async () => {
    console.warn("[luna:native] runElevatedInstall() is not supported in TidaLunar");
};
