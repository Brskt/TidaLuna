// Receiver-side `media_changed` enrichment: the payload only carries a
// `mediaId`, so we fetch the full track record from the TIDAL API before
// dispatching Redux actions, updating the player bar, and injecting the
// cover image. Isolated from the pure relay listeners in `index.ts` so the
// listener stays a relay and the network/DOM side-effects live here.

import { tidalFetch } from "../ipc";
import type { ReceiverMediaChangedPayload } from "./types";

type Store = {
    dispatch: (action: { type: string; payload?: unknown }) => void;
    getState: () => any;
};

const COVER_ID_RE = /^[0-9a-f-]+$/i;

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
): Promise<void> {
    const trackId = data?.mediaId;
    if (!trackId) return;

    try {
        const cid: string | undefined = store.getState()?.session?.clientId;
        const cc: string = store.getState()?.session?.countryCode ?? "US";
        const lang: string = store.getState()?.settings?.language ?? "en_US";
        const url = `https://desktop.tidal.com/v1/tracks/${trackId}?countryCode=${cc}&deviceType=DESKTOP&locale=${lang}`;

        const res = await tidalFetch(
            url,
            cid ? { headers: { "x-tidal-token": cid } } : undefined,
        );

        if (res.status < 200 || res.status >= 300) return;

        const track = await res.json();
        const cover: string | undefined = track?.album?.cover;
        const imageUrl = cover
            ? `https://resources.tidal.com/images/${cover.replace(/-/g, "/")}/1280x1280.jpg`
            : undefined;

        // Inject into content cache so selectors can find it.
        store.dispatch({
            type: "content/LOAD_SINGLE_MEDIA_ITEM_SUCCESS",
            payload: { mediaItem: { type: "track", item: track } },
        });

        // Update Redux state (player bar React components read from here).
        const controls = store.getState().playbackControls ?? {};
        const oldMp = controls.mediaProduct ?? {};
        store.dispatch({
            type: "playbackControls/MEDIA_PRODUCT_TRANSITION",
            payload: {
                mediaProduct: {
                    ...oldMp,
                    productId: String(trackId),
                    productType: "track",
                },
                playbackContext: {
                    ...controls.playbackContext,
                    actualProductId: String(trackId),
                    actualDuration:
                        track.duration ?? (data?.metadata?.duration ?? 0) / 1000,
                    actualAudioQuality: track.audioQuality ?? "LOSSLESS",
                },
            },
        });

        // Feed through the normal playback path (MediaSession, Rust metadata,
        // interceptors).
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
        (window as unknown as { nativeInterface?: { playback?: { setCurrentMediaItem?: (i: unknown) => void } } })
            .nativeInterface
            ?.playback
            ?.setCurrentMediaItem
            ?.(item);

        // Set cover via background-image CSS (doesn't interfere with React's
        // DOM). TIDAL's SDK manages its own cover <img> but doesn't render it
        // for externally-driven playback (Connect receiver).
        applyCoverBackground(cover);
    } catch {
        // API fetch failed - non-critical, player bar will not update.
    }
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
