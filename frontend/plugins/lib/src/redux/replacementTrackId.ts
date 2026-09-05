import type { ItemId } from "./types";
import type { TidalStoreState } from "./types/store";

/**
 * The track TIDAL serves in place of `itemId`, when its catalogue has named one.
 *
 * A catalogue drop does not remove the row from a playlist: TIDAL leaves a resource whose
 * `replacement` relationship names the id that survives. Every endpoint then refuses the
 * original (`/tracks/{id}`, its `/playbackinfo`, and the v2 resource alike), and a lookup that
 * knows only the original id has nowhere left to ask, while the replacement resolves normally
 * and is what the player actually streams.
 *
 * A rule rather than a lookup inside its caller, because the caller reaches it through
 * `MediaItem`, which cannot be imported under the test runner at all: the rule would have been
 * unreachable by any test, and this one is worth pinning: it decides which id a row describes.
 *
 * Free at the call site: the resource is already in the store, put there to render the row.
 */
export const replacementTrackId = (state: TidalStoreState, itemId: ItemId): ItemId | undefined => {
	const entity = state.entities?.tracks?.entities?.[String(itemId)];
	const replacement = entity?.relationships?.replacement?.data;
	// One check for absent, empty and wrong-typed, because JSON:API spells "no replacement" as
	// an explicit `data: null` rather than by leaving the relationship out; a track that
	// simply has not been replaced arrives here as null, not undefined. Typed as well as
	// present: the same relationship shape carries albums and artists, and a non-track id handed
	// to a track lookup would resolve to something else entirely rather than fail.
	if (replacement?.type !== "tracks") return undefined;
	// A resource naming itself would send the caller round again for the id that just failed.
	return String(replacement.id) === String(itemId) ? undefined : replacement.id;
};
