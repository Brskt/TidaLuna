import type { ItemId } from "../redux/types";

/**
 * Whether two ids name the same item, whatever each of them is spelled as.
 *
 * The same id reaches us as a number from the play queue and as a string from the store, which
 * keys by `String(id)`, and from TIDAL's JSON:API resources, where ids are strings by the
 * specification. A `===` between two of those is false for the same track, silently: nothing
 * throws, a row simply stops recognising itself.
 *
 * `undefined` matches nothing, including another `undefined`: two unknowns are not a pair.
 */
export const sameItemId = (a: ItemId | undefined, b: ItemId | undefined): boolean => a !== undefined && b !== undefined && String(a) === String(b);
