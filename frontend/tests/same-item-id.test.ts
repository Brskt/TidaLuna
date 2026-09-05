// Tests `plugins/lib/src/helpers/sameItemId.ts`.
//
// Ids reach this library in three spellings for one track: a number from the play queue, a
// string from `content.mediaItems` (which keys by `String(id)`), and a string again from the
// JSON:API resources, where the specification requires it. A `===` between two of those is
// false for the same track and throws nothing: a row just quietly stops being itself.
//
// It matters most where a row has TWO ids: when TIDAL's catalogue drops a track it hands back
// a replacement; the row is called by one id and served under another, and it must answer
// to both or it never recognises itself as the track that is playing.

import { expect, test } from "bun:test";

import { sameItemId } from "../plugins/lib/src/helpers/sameItemId";

test("a number and its string spelling name the same item", () => {
	expect(sameItemId(103046509, "103046509")).toBe(true);
	expect(sameItemId("103046509", 103046509)).toBe(true);
});

test("the same spelling on both sides still matches", () => {
	expect(sameItemId(103046509, 103046509)).toBe(true);
	expect(sameItemId("103046509", "103046509")).toBe(true);
});

test("different items do not match, however they are spelled", () => {
	expect(sameItemId(103046509, 62416116)).toBe(false);
	expect(sameItemId("103046509", 62416116)).toBe(false);
});

test("an unknown id matches nothing, including another unknown", () => {
	// Two unknowns are not a pair: an absent current-track id must not read as "this is the
	// track playing" for every row on screen.
	expect(sameItemId(undefined, 103046509)).toBe(false);
	expect(sameItemId(103046509, undefined)).toBe(false);
	expect(sameItemId(undefined, undefined)).toBe(false);
});
