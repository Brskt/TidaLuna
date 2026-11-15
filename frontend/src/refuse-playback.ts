import { isStoreNotPublished } from "../plugins/lib/src/redux/storeNotPublished";
import { proxyFail, proxyRetry } from "./audio-proxy";
import { refusalMessage, showErrorBanner } from "./banner";

// What a refusal leaves behind, until Rust holds the product again. `standing`: the listener has not
// asked for it again. `retrying`: they have, and the load that answers has not reached Rust.
type Refusal = { productId: string; stage: "standing" | "retrying" };
let refusal: Refusal | null = null;

// What a Play means while a refusal may stand. `resume`: Rust holds the track the screen names; the
// Play is Rust's. `retry`: it asks for the refused product again, and is held back until it is loaded.
// `wait`: it already did, and the track Rust holds meanwhile is not the one the Play is for.
export type PlayOnRefusal = "resume" | "retry" | "wait";

// Takes back the Play `holdPlay` keeps for the retry.
let releasePlay: () => void = () => {};

// The Play of a retry waits for the product to be loaded, and then goes out as the listener's own:
// Rust holds the track, and TIDAL's reload of it has made the transition that clears `prefilled`.
// It is dropped when the retry fails, or when another track is announced first.
const holdPlay = (productId: string): void => {
    releasePlay();
    const { store } = require("../plugins/lib/src/redux/store");
    const unsubscribe = store.subscribe(() => {
        const controls = store.getState().playbackControls;
        if (refusal !== null || controls?.prefilled) return;
        if (String(controls?.mediaProduct?.productId ?? "") !== productId) return;
        releasePlay();
        store.dispatch({ type: "playbackControls/PLAY" });
    });
    releasePlay = () => {
        unsubscribe();
        releasePlay = () => {};
    };
};

// Another track was announced: what a refusal named is no longer what a Play asks for. The refused
// product announced again is the retry's own announcement, and leaves the refusal as it is.
export function forgetRefusal(announced: string): void {
    if (refusal?.productId === announced) return;
    refusal = null;
    releasePlay();
}

function refusalOfCurrent(): Refusal | null {
    if (refusal === null) return null;
    const { store } = require("../plugins/lib/src/redux/store");
    const current = String(store.getState().playbackControls?.mediaProduct?.productId ?? "");
    return current === refusal.productId ? refusal : null;
}

/**
 * Whether the product the listener has is one this page refused and Rust does not hold yet. Rust's
 * track is then the one the refusal left behind; a command that acts on it (a resume, a seek)
 * acts on a track the screen does not name.
 */
export function refusalStands(): boolean {
    return refusalOfCurrent() !== null;
}

/**
 * The listener asks for the refused product again: the SDK starts loading it, or a Play asks. One
 * request per refusal, and the SDK's stream for it, withheld while the refusal stood, is announced
 * ready again. True when the product was the refused one and had not been asked for yet.
 */
export function askRefusedAgain(productId: string): boolean {
    if (refusal?.stage !== "standing" || refusal.productId !== productId) return false;
    refusal = { productId, stage: "retrying" };
    proxyRetry();
    return true;
}

// Rust reported the length of a track it loaded: when that was the retry's, it holds the product again. A
// length that names another track (the track Rust kept re-reporting its own) is not.
export function refusalDelivered(product?: string): void {
    if (refusal?.stage === "retrying" && (product === undefined || product === refusal.productId)) refusal = null;
}

/**
 * The listener's Play while a refusal may stand. TIDAL's Play on a stopped transport reloads the
 * current product, and that is the retry, but only the page's own request loads a track into Rust,
 * and TIDAL's PLAY saga waits for the transition of the load the refusal left pending, which never
 * comes: the Play would sit in the saga and start whatever is loaded next. Instead the Play is held
 * back here (`retry` and `wait` cancel it), the product is announced again, and TIDAL is asked to
 * reload the current product itself: its stream binds the SDK's player again, and its transition
 * releases the held Play.
 */
export function playOnRefusal(): PlayOnRefusal {
    const standing = refusalOfCurrent();
    if (standing === null) return "resume";
    if (standing.stage === "retrying") return "wait";
    askRefusedAgain(standing.productId);
    holdPlay(standing.productId);
    window.nativeInterface.playback.replaySelfLoad();
    const { store } = require("../plugins/lib/src/redux/store");
    store.dispatch({ type: "playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE" });
    return "retry";
}

/**
 * Refuse the track that is loading or playing, and tell the listener why.
 *
 * Every step runs even when an earlier one fails. A store that is not published yet is the one
 * failure that is no failure: the refusal is then the element's alone, and the steps that needed
 * the store are dropped, not queued (a STOP landing after the store appears would stop whatever
 * the listener has started since). Anything else that goes wrong is raised after the last step,
 * for the caller's batch to report.
 *
 * TIDAL arms a one second watchdog on every autoplay load: unless something ends it, it writes
 * SET_PLAYBACK_STATE("STALLED"), whose reducer forces the desired state back to PLAYING, and the
 * spinner never clears. The STOP ends it once `end_stall_watchdog` (module_capture.rs) has rewritten it.
 * The `player/ERROR` ends it on a build where that rewrite went inert, and only after the IDLE: a
 * watchdog parked on that IDLE takes no error before it.
 *
 * `productId` is the product this page could not load, which the listener's Play asks for again
 * (`playOnRefusal`). A refusal that is not this page's load failing names none: a cast owning the
 * player and a lost network leave Rust with the track a Play must go on resuming. `cause` is the
 * device failure that ended the load, when one did (`refusalMessage`).
 */
export function refusePlayback(message: string, productId = "", cause?: string): void {
    refusal = productId === "" ? null : { productId, stage: "standing" };
    releasePlay();
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
    // TIDAL's own pairing for a failure on the current track. STOP is grouped with
    // PAUSE in the reducer, never with SKIP_NEXT. It also stops Rust's track when this page
    // loaded it (the STOP interceptor in index.ts): the SDK's reset() leaves that one playing.
    attempt(() => store().dispatch({ type: "playbackControls/STOP" }));
    // Clears a STALLED already posted, without rewriting the intent the way
    // PLAYING and STALLED both do.
    attempt(() => store().dispatch({ type: "playbackControls/SET_PLAYBACK_STATE", payload: "IDLE" }));
    // No `errorCode`: TIDAL's error saga then only logs, where a code would raise a banner of its
    // own or, for `S3016`, advance the queue. Its reducer reads the payload, which must exist.
    attempt(() => store().dispatch({ type: "player/ERROR", payload: { errorCode: undefined, errorId: "EUnexpected" } }));
    attempt(() => showErrorBanner(store(), "PLAYBACK", refusalMessage(store, message, cause)));

    if (storeMissing) console.warn("[refusePlayback] store not published: STOP, IDLE, ERROR and banner dropped");
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) throw new AggregateError(failures, "[refusePlayback] several steps failed");
}
