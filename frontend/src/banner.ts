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
