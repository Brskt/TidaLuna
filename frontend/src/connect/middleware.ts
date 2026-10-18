// Receiver-side `media_changed` enrichment: the payload only carries a
// `mediaId`; we fetch the full track record from the TIDAL API before
// dispatching Redux actions, updating the player bar, and injecting the
// cover image. Isolated from the pure relay listeners in `index.ts`, keeping the
// listener a relay while the network/DOM side-effects live here.

import { tidalFetch } from "../ipc";
import { isStoreNotPublished } from "../../plugins/lib/src/redux/storeNotPublished";
import { storePublished } from "../store-published";
import type { ReceiverMediaChangedPayload } from "./types";

type Store = {
    dispatch: (action: { type: string; payload?: unknown }) => void;
    getState: () => any;
};

const COVER_ID_RE = /^[0-9a-f-]+$/i;

// The receiver's latest announcement. Each one waits on its own track lookup, and a lookup can
// land after a later one's: applied then, it would make a track the cast has left this page's queue.
let latestAnnouncement = 0;

/**
 * Handle `connect.receiver.media_changed`: enrich with a tidalFetch lookup,
 * then dispatch the Redux actions + DOM updates that drive the player bar.
 *
 * Silently no-ops on any upstream error; the player bar simply does not
 * update, which matches the pre-refactor behaviour.
 */
export async function handleReceiverMediaChanged(
    store: Store,
    data: ReceiverMediaChangedPayload,
    published: () => Promise<void> = storePublished,
): Promise<void> {
    const trackId = data?.mediaId;
    if (!trackId) return;
    const announcement = ++latestAnnouncement;

    try {
        let state: any;
        try {
            state = store.getState();
        } catch (e) {
            if (!isStoreNotPublished(e)) throw e;
            // A cast during boot lands before the store does: the announcement waits for it,
            // unless a later one took its place meanwhile.
            await published();
            if (announcement !== latestAnnouncement) return;
            state = store.getState();
        }
        const cid: string | undefined = state?.session?.clientId;
        const cc: string = state?.session?.countryCode ?? "US";
        const lang: string = state?.settings?.language ?? "en_US";
        const url = `https://desktop.tidal.com/v1/tracks/${trackId}?countryCode=${cc}&deviceType=DESKTOP&locale=${lang}`;

        const res = await tidalFetch(
            url,
            cid ? { headers: { "x-tidal-token": cid } } : undefined,
        );

        if (res.status < 200 || res.status >= 300) return;

        const track = await res.json();
        if (announcement !== latestAnnouncement) return;
        const cover: string | undefined = track?.album?.cover;
        const imageUrl = cover
            ? `https://resources.tidal.com/images/${cover.replace(/-/g, "/")}/1280x1280.jpg`
            : undefined;

        // Inject into content cache for selectors to find it: TIDAL's queue names its current
        // item as a media product only once the item is there.
        store.dispatch({
            type: "content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS",
            payload: { mediaItem: { type: "track", item: track } },
        });

        // Feed through the normal playback path (MediaSession, interceptors), as a track already
        // playing: the receiver loaded it, and a self-load of it here would be refused, under the
        // banner that names the cast. Before the queue changes: TIDAL announces the queue's new
        // item through this same call without the second argument, and that call then matches
        // `lastTransitionId` and loads nothing.
        const item = {
            id: track.id,
            productId: track.id,
            title: track.title,
            artist: track.artists?.[0]?.name ?? track.artist?.name ?? "",
            album: track.album?.title ?? "",
            imageUrl,
            duration: track.duration,
            type: "track",
        };
        (window as unknown as { nativeInterface?: { playback?: { setCurrentMediaItem?: (i: unknown, o: { alreadyPlaying: boolean }) => void } } })
            .nativeInterface
            ?.playback
            ?.setCurrentMediaItem
            ?.(item, { alreadyPlaying: true });

        castTrackBecomesQueue(store, track.id);

        // Set cover via background-image CSS (doesn't interfere with React's
        // DOM). TIDAL's SDK manages its own cover <img> but doesn't render it
        // for externally-driven playback (Connect receiver).
        applyCoverBackground(cover);
    } catch {
        // API fetch failed - non-critical, player bar will not update.
    }
}

/**
 * Make the receiver's track this page's play queue, as TIDAL's own receiver does for a
 * controller's track (`handleRemotePlaybackMediaChanged`: one element, `pq_0`). TIDAL loads it
 * without playing and Rust settles that load as the cast's track: after the cast, a Play resumes
 * it where the cast left it.
 */
function castTrackBecomesQueue(store: Store, mediaItemId: unknown): void {
    // TIDAL's own element for an item of no known source. `originalSource` too: left to the
    // reducer, it keeps the queue from before the cast, which a wrap at the queue's end with
    // repeat on, or shuffle turned off, would bring back.
    const element = { context: { type: "UNKNOWN" }, mediaItemId, priority: "priority_none", uid: "pq_0" };
    store.dispatch({
        type: "playQueue/RESET",
        payload: { currentIndex: 0, elements: [element], originalSource: [element] },
    });
    // TIDAL's sagas run inside that dispatch, and its load prefills the bar first (`prefilled`).
    // It skips an item its saved restart state names, whatever its player holds now. The reload it
    // runs after a purchase has no such shortcut.
    if (!store.getState()?.playbackControls?.prefilled) reloadQueueItem(store);
}

// TIDAL's reload of the play queue's current item into its player, not playing it.
const reloadQueueItem = (store: Store) =>
    store.dispatch({ type: "playbackControls/REFRESH_PLAYBACK_AFTER_PURCHASE" });

/**
 * A load TIDAL made on its own, refused during a cast; its player now holds that load's track.
 * For a track other than the queue's (a late load of one the phone already left), the queue's
 * track is reloaded: a Play after the cast resumes it. A refused load of the queue's own track
 * reloads nothing, which bounds this.
 */
export function followCastQueue(store: Store, loadedProductId: string | null): void {
    const queue = store.getState()?.playQueue;
    const current = queue?.elements?.[queue.currentIndex]?.mediaItemId;
    if (current === undefined || String(current) === String(loadedProductId)) return;
    reloadQueueItem(store);
}

function applyCoverBackground(cover: string | undefined) {
    if (!cover || !COVER_ID_RE.test(cover)) return;
    const thumbUrl = `https://resources.tidal.com/images/${cover.replace(/-/g, "/")}/80x80.jpg`;
    const fig = document.querySelector(
        '[data-test="current-media-imagery"]',
    ) as HTMLElement | null;
    if (!fig) return;
    fig.style.backgroundImage = `url("${thumbUrl}")`;
    fig.style.backgroundSize = "cover";
    fig.style.backgroundPosition = "center";
    fig.style.borderRadius = "4px";
}
