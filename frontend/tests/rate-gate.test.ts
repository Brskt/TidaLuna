// Tests `plugins/lib/src/helpers/RateGate.ts`, the ceiling on how fast this library asks TIDAL
// for anything.
//
// It exists because removing a global lock from the media-item lookup traded one problem for
// another: rows that used to resolve one per five seconds all left at once instead, dozens of
// requests inside a second. The gate keeps the concurrency and spends it evenly.
//
// Rated high on purpose here (100/s = 10 ms apart) to keep the timing real and the suite
// fast; the production rate is a constructor argument, not a constant this file re-asserts.

import { expect, test } from "bun:test";

import { RateGate } from "../plugins/lib/src/helpers/RateGate";

test("concurrent callers come out spaced, not in a burst", async () => {
	const gate = new RateGate(100);
	const started = performance.now();

	const at: number[] = [];
	await Promise.all(
		Array.from({ length: 5 }, async () => {
			await gate.pass();
			at.push(performance.now() - started);
		}),
	);

	// Five slots at 10 ms apart: the last one cannot have left before the fourth interval. A
	// burst would put every entry at ~0.
	expect(at).toHaveLength(5);
	expect(Math.max(...at)).toBeGreaterThanOrEqual(35);
});

test("the most recently asked-for caller is served first", () => {
	// The reason this class is a queue at all. Rows enter the DOM as they are scrolled past:
	// arrival order is the order the listener has already LEFT; served that way, what is on
	// screen waits behind everything above it. Measured at 46 seconds of queue on a real
	// playlist, which reads as "it stopped loading".
	const gate = new RateGate(100);
	const served: number[] = [];

	const all = [1, 2, 3, 4].map((n) => gate.pass().then(() => void served.push(n)));

	return Promise.all(all).then(() => {
		// All four are queued before the gate releases any of them: the order out is the
		// exact reverse of the order in.
		expect(served).toEqual([4, 3, 2, 1]);
	});
});

test("a caller held back is served eventually, never dropped", async () => {
	// Newest-first must not mean the oldest is abandoned: a row scrolled past still fills in
	// once the queue drains; no row is permanently blank for having been unlucky.
	const gate = new RateGate(200);
	const first = gate.pass();
	const rest = Array.from({ length: 20 }, () => gate.pass());

	await Promise.all([first, ...rest]);
	expect(gate.queued).toBe(0);
});

test("after an idle spell, two callers are served within a few intervals", async () => {
	const gate = new RateGate(100);
	await gate.pass();
	await new Promise((resolve) => setTimeout(resolve, 60));

	const started = performance.now();
	await gate.pass();
	await gate.pass();

	// The first goes out at once and the second one interval (10 ms) later.
	expect(performance.now() - started).toBeLessThan(40);
});

test("a wall clock put back does not hold the gate shut", async () => {
	// The clock of a VM is corrected by steps, backwards among them. A release stamped from it
	// would leave the next caller waiting out the whole step: here three seconds for a gate that
	// asked for ten milliseconds.
	const realNow = Date.now;
	const gate = new RateGate(100);
	await gate.pass();
	await new Promise((resolve) => setTimeout(resolve, 60));

	Date.now = () => realNow() - 3000;
	try {
		const started = performance.now();
		await gate.pass();
		expect(performance.now() - started).toBeLessThan(500);
	} finally {
		Date.now = realNow;
	}
});
