import { proxyFail } from "./audio-proxy";

/**
 * Refuse the track that is loading or playing, and tell the listener why.
 *
 * The five steps are one unit and the order is load-bearing, found after five
 * attempts that each looked like a fix. TIDAL arms a one second
 * race on every autoplay load: without an error it writes SET_PLAYBACK_STATE("STALLED"),
 * whose reducer forces the desired state back to PLAYING; a STOP posted alone is
 * undone a second later and the spinner never clears.
 *
 * `errorCode` must stay OUTSIDE TIDAL's three-key map (`file_checksum_mismatch`,
 * `no_such_file`, `unreadable_file`). A mapped code raises a second banner beside ours,
 * and `S3016` is the one code that advances the queue, which is the opposite of
 * refusing. An unknown code takes the log-only branch while still raising
 * `player/ERROR`, which is what wins the race.
 */
export function refusePlayback(errorCode: string, error: string, message: string): void {
    let store: { dispatch: (action: unknown) => void };
    try {
        ({ store } = require("../plugins/lib/src/redux/store"));
    } catch (_) {
        // No store yet means nothing is playing to refuse, and the banner has nowhere
        // to land. `proxyFail` below is what actually stops the element re-arming.
        proxyFail();
        return;
    }

    // The element stops announcing itself ready, or the spinner starts over.
    proxyFail();
    // `mediaerror` is the sanctioned channel: the SDK listens on this component and
    // reads `e.target.errorCode`.
    (window as { NativePlayerComponent?: { trigger?: (name: string, payload: unknown) => void } })
        .NativePlayerComponent?.trigger?.("mediaerror", { error, errorCode });
    // TIDAL's own pairing for a failure on the current track. STOP is grouped with
    // PAUSE in the reducer, never with SKIP_NEXT.
    store.dispatch({ type: "playbackControls/STOP" });
    // Clears a STALLED already posted, without rewriting the intent the way
    // PLAYING and STALLED both do.
    store.dispatch({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" });
    // The red banner at the bottom of the screen.
    store.dispatch({
        type: "message/MESSAGE_ERROR",
        payload: {
            id: Date.now(),
            category: "PLAYBACK",
            severity: "ERROR",
            message,
        },
    });
}
