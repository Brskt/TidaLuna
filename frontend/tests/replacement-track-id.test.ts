// Tests `plugins/lib/src/redux/replacementTrackId.ts`, the rule that decides which id a
// playlist row actually describes.
//
// Observed on a live session: track 103046509 sits in a playlist, plays fine, and yet
// `/v1/tracks/103046509`, its `/playbackinfo`, and the v2 resource ALL refuse it. What the
// player streams is 62416116: the id its `replacement` relationship names. A lookup that
// knows only the row's own id therefore has nothing left to ask, which is why the row carried
// no quality tag while the transport happily showed the format it was decoding.
//
// The fixtures are the real shape, copied from that session's store.

import { expect, test } from "bun:test";

import { replacementTrackId } from "../plugins/lib/src/redux/replacementTrackId";

const stateWith = (entities: Record<string, unknown>) => ({ entities: { tracks: { entities } } }) as any;

const withReplacement = (id: string, replacedBy: string, type = "tracks") => ({
	id,
	type: "tracks",
	relationships: {
		albums: { data: [{ id: "103046508", type: "albums" }] },
		artists: { data: [{ id: "7284579", type: "artists" }] },
		replacement: { data: { id: replacedBy, type } },
	},
});

test("a dropped track hands back the id TIDAL serves in its place, however the caller spells its id", () => {
	// The store keys by `String(id)` while callers hold whatever the queue gave them, which for
	// a track is a number. Indexing with the raw value would miss every time.
	const state = stateWith({ "103046509": withReplacement("103046509", "62416116") });

	for (const id of [103046509, "103046509"]) {
		expect(replacementTrackId(state, id), `${typeof id} id`).toBe("62416116");
	}
});

test("a relationship naming something other than a track is refused", () => {
	// The same shape carries albums and artists. A non-track id handed to a track lookup would
	// resolve to a different thing entirely rather than fail.
	const state = stateWith({ "103046509": withReplacement("103046509", "103046508", "albums") });

	expect(replacementTrackId(state, 103046509)).toBeUndefined();
});

test("a resource naming itself does not send the caller round again", () => {
	const state = stateWith({ "103046509": withReplacement("103046509", "103046509") });

	expect(replacementTrackId(state, 103046509)).toBeUndefined();
});
