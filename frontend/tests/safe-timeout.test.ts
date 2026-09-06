// Tests `plugins/lib/src/helpers/safeTimeout.ts`.
//
// The set these helpers register into is the plugin's own, and it lives as long as the plugin
// does. A timeout is spent the moment it fires: a registration left behind can only ever be a
// no-op that nobody will ever need. These helpers are called from paths that run once per row
// of a list, and the no-ops accumulate for the whole session.
//
// An interval is the opposite case and the tests below pin the difference: it is still running
// after it fires, and its registration is the only way anything can ever stop it.

import { expect, test } from "bun:test";

import { safeInterval, safeTimeout } from "../plugins/lib/src/helpers/safeTimeout";

const set = () => new Set<any>();

test("a timeout that has fired is no longer registered", async () => {
	const unloads = set();
	safeTimeout(unloads, () => {}, 5);
	expect(unloads.size).toBe(1);

	await new Promise((res) => setTimeout(res, 40));

	expect(unloads.size).toBe(0);
});

test("a timeout cancelled before it fires is no longer registered either", () => {
	const unloads = set();
	const unload = safeTimeout(unloads, () => {}, 10_000);
	expect(unloads.size).toBe(1);

	unload();

	expect(unloads.size).toBe(0);
});

// The distinction that makes this family two families: deregistering an interval when it fires
// would drop the only handle anything has on a timer that is still running.
test("an interval that has fired keeps its registration, because it is still running", async () => {
	const unloads = set();
	const unload = safeInterval(unloads, () => {}, 5);

	await new Promise((res) => setTimeout(res, 40));

	expect(unloads.size).toBe(1);
	unload();
	expect(unloads.size).toBe(0);
});
