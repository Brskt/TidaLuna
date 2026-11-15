// The text key TIDAL's error saga shows for each device failure its native player reports, by the
// event name Rust emits for it. A refusal showing the same key is dropped by TIDAL's banner reducer
// when TIDAL's own banner already stands, and the other way round.
const DEVICE_FAILURE_KEYS: Record<string, string> = {
    deviceexclusivemodenotallowed: "t-device-exclusive-move-not-allowed",
    deviceformatnotsupported: "t-output-format-not-supported",
    devicelocked: "t-media-locked",
    devicenotfound: "t-device-not-found",
    deviceunknownerror: "t-device-unkown-error",
};

/**
 * What a refusal's banner says: TIDAL's key for the device failure `cause` names, while TIDAL's
 * dictionary still holds it (a renamed key would show raw), else `message`. Never throws: a store
 * not published yet has no dictionary to read.
 */
export function refusalMessage(store: () => { getState: () => any }, message: string, cause?: string): string {
    const key = cause === undefined ? undefined : DEVICE_FAILURE_KEYS[cause];
    if (key === undefined) return message;
    try {
        const locale = store().getState()?.locale;
        // TIDAL renders its English dictionary overlaid by the current one, English always loaded.
        for (const bundle of ["en-us", locale?.currentBundleName]) {
            const text = locale?.bundles?.[bundle]?.i18n?.[key];
            if (typeof text === "string" && text !== "") return key;
        }
    } catch (_) {}
    return message;
}

/**
 * TIDAL clears a "PLAYBACK" banner when the next track starts playing; an "OTHER" one stays until
 * the listener closes it.
 */
export function showErrorBanner(store: { dispatch: (action: unknown) => void }, category: "PLAYBACK" | "OTHER", message: string): void {
    store.dispatch({
        type: "message/MESSAGE_ERROR",
        payload: {
            id: Date.now(),
            category,
            severity: "ERROR",
            message,
        },
    });
}
