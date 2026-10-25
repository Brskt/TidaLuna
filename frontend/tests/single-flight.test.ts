// Tests `plugins/lib/src/helpers/SingleFlight.ts`, which stops the same lookup being run more
// than once at a time.
//
// The window it closes: `ContentBase._instances` fills only when a lookup RESOLVES. Between
// the ask and the answer it says nothing, and every caller arriving meanwhile started its own
// lookup. Rows leave and re-enter the DOM constantly while a list is scrolled, and they ask
// again. Measured on a 397-track playlist: 2347 queued requests for 397 tracks, of which the
// memoized fetches account for at most 794. The remainder is about four load dispatches per
// track, three of them for an answer their caller would throw away, and TIDAL served each one.

import { expect, test } from "bun:test";

import { SingleFlight } from "../plugins/lib/src/helpers/SingleFlight";

const deferred = <T>() => {
	const { promise, resolve, reject } = Promise.withResolvers<T>();
	return { promise, resolve, reject };
};

test("callers arriving during a run join it instead of starting another", async () => {
	const flight = new SingleFlight<string>();
	const gate = deferred<string>();
	let runs = 0;

	const work = () => {
		runs++;
		return gate.promise;
	};
	const all = Promise.all([flight.run("track:1", work), flight.run("track:1", work), flight.run("track:1", work)]);
	expect(flight.inFlight).toBe(1);

	gate.resolve("answered once");
	expect(await all).toEqual(["answered once", "answered once", "answered once"]);
	expect(runs).toBe(1);
});

test("a key is free again once its run settles, or fails", async () => {
	// Not a cache: it holds work, not answers; whoever asks after a run settled gets a fresh one,
	// which lets `_instances` be what remembers. A failure left behind would be handed to every later
	// caller for the life of the page: the single outcome worse than repeating the work.
	for (const [what, first, outcome] of [
		["a run that settles", async () => "first answer", "resolved first answer"],
		["a failed run", () => Promise.reject(new Error("refused")), "rejected refused"],
	] as const) {
		const flight = new SingleFlight<string>();

		const settled = await flight.run("track:1", first).then(
			(value) => `resolved ${value}`,
			(error: Error) => `rejected ${error.message}`,
		);
		expect(settled, what).toBe(outcome);
		expect(flight.inFlight, what).toBe(0);

		expect(await flight.run("track:1", async () => "second chance"), what).toBe("second chance");
	}
});
