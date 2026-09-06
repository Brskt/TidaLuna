/**
 * TIDAL's newer normalised cache, a sibling of `content` at the store root.
 *
 * Two caches now hold tracks. The legacy `content.mediaItems` is written by the sagas and holds
 * the full item; this one is filled by RTK-Query from the JSON:API endpoints and holds the raw
 * resource. Playlist views read from here, which is why a row can be on screen while
 * `content.mediaItems` has never heard of it.
 *
 * Deliberately modelled down to what we read and no further. The real payload carries far more
 * (`attributes` alone holds title, isrc, mediaTags, bpm, key, duration as ISO-8601), and every
 * field named here is one we could check against a live payload. Declaring the rest from the
 * shape of a neighbouring endpoint's type is how the last set of wrong conclusions got made.
 */
export interface Entities {
	tracks?: EntityCache<EntityTrack>;
}

export interface EntityCache<T> {
	entities?: Record<string, T | undefined>;
}

/** A JSON:API track resource as RTK-Query stores it. */
export interface EntityTrack {
	id: string;
	type: string;
	relationships?: {
		/**
		 * The track TIDAL serves in this one's place. Present when the catalogue has dropped the
		 * original for this region: every endpoint then refuses the original id, while the
		 * replacement resolves normally and is what actually plays.
		 */
		replacement?: EntityRef;
		albums?: EntityRefList;
		artists?: EntityRefList;
	};
}

/** `null` is JSON:API's spelling of an empty to-one relationship, and it is the common case. */
export interface EntityRef {
	data?: { id: string; type: string } | null;
}

export interface EntityRefList {
	data?: { id: string; type: string }[] | null;
}
