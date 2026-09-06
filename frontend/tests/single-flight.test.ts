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

test("different keys do not wait on each other", async () => {
	const flight = new SingleFlight<string>();
	const one = deferred<string>();
	const two = deferred<string>();

	const first = flight.run("track:1", () => one.promise);
	const second = flight.run("track:2", () => two.promise);
	expect(flight.inFlight).toBe(2);

	// The second answers while the first is still out: one key cannot hold another back.
	two.resolve("two");
	expect(await second).toBe("two");

	one.resolve("one");
	expect(await first).toBe("one");
});

test("a key is free again once its run settles", async () => {
	// Not a cache: it holds work, not answers. Whoever asks after the answer landed gets a
	// fresh run, which is what lets `_instances` be the thing that remembers.
	const flight = new SingleFlight<number>();
	let runs = 0;

	expect(await flight.run("track:1", async () => ++runs)).toBe(1);
	expect(flight.inFlight).toBe(0);
	expect(await flight.run("track:1", async () => ++runs)).toBe(2);
});

test("a failed run frees its key rather than poisoning it", async () => {
	// Left behind, one failure would be handed to every later caller for the life of the page:
	// the single outcome worse than repeating the work.
	const flight = new SingleFlight<string>();

	await expect(flight.run("track:1", () => Promise.reject(new Error("refused")))).rejects.toThrow("refused");
	expect(flight.inFlight).toBe(0);

	expect(await flight.run("track:1", async () => "second chance")).toBe("second chance");
});
