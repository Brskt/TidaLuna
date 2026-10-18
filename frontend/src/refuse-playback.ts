import { isStoreNotPublished } from "../plugins/lib/src/redux/storeNotPublished";
import { proxyFail } from "./audio-proxy";
import { showErrorBanner } from "./banner";

/**
 * Refuse the track that is loading or playing, and tell the listener why.
 *
 * Every step runs even when an earlier one fails. A store that is not published yet is the one
 * failure that is no failure: the refusal is then the element's alone, and the steps that needed
 * the store are dropped, not queued (a STOP landing after the store appears would stop whatever
 * the listener has started since). Anything else that goes wrong is raised after the last step,
 * for the caller's batch to report.
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
    const failures: unknown[] = [];
    let storeMissing = false;
    const attempt = (step: () => void) => {
        try {
            step();
        } catch (e) {
            if (isStoreNotPublished(e)) storeMissing = true;
            else failures.push(e);
        }
    };
    // The store is a lazy proxy that throws on first use until it is published: the `require`
    // itself never fails and each use is what throws.
    const store = () => require("../plugins/lib/src/redux/store").store;

    // The element stops announcing itself ready, or the spinner starts over.
    proxyFail();
    // `mediaerror` is the sanctioned channel: the SDK listens on this component and
    // reads `e.target.errorCode`.
    attempt(() =>
        (window as { NativePlayerComponent?: { trigger?: (name: string, payload: unknown) => void } })
            .NativePlayerComponent?.trigger?.("mediaerror", { error, errorCode }),
    );
    // TIDAL's own pairing for a failure on the current track. STOP is grouped with
    // PAUSE in the reducer, never with SKIP_NEXT. It also stops Rust's track when this page
    // loaded it (the STOP interceptor in index.ts): the SDK's reset() leaves that one playing.
    attempt(() => store().dispatch({ type: "playbackControls/STOP" }));
    // Clears a STALLED already posted, without rewriting the intent the way
    // PLAYING and STALLED both do.
    attempt(() => store().dispatch({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" }));
    attempt(() => showErrorBanner(store(), "PLAYBACK", message));

    if (storeMissing) console.warn("[refusePlayback] store not published: STOP, IDLE and banner dropped");
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) throw new AggregateError(failures, "[refusePlayback] several steps failed");
}
