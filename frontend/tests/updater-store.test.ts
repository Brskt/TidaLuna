import { describe, expect, test } from "bun:test";

import {
	eventForDownloadFailure,
	hydrateUpdater,
	initialUpdaterState,
	reduceUpdater,
	type UpdaterState,
} from "../plugins/ui/src/updater/state";

// `satisfies` rather than an annotation: the record is a union keyed on `phase`, and widening
// the fixture to the whole union would lose the variant every `{ ...ready, phase }` below
// builds from.
const ready = {
	info: { version: "1.2.3", download_size: 4096 },
	phase: "ready",
	errorMsg: "",
	withheldReason: "",
} satisfies UpdaterState;

describe("the updater state every surface shares", () => {
	// The two surfaces disagreed here too: one reset to "available" on any cancel, the other
	// only from "downloading". Against an apply already claimed, the loose reading offers the
	// update again while the app is on its way out.
	test("a cancel only undoes a download", () => {
		const downloading: UpdaterState = { ...ready, phase: "downloading" };
		expect(reduceUpdater(downloading, { kind: "cancelled" }).phase).toBe("available");

		const applying: UpdaterState = { ...ready, phase: "applying" };
		expect(reduceUpdater(applying, { kind: "cancelled" }).phase).toBe("applying");
	});

	// A check answers while a download or an apply may already be acting on the offer, and taking
	// the record back mid-operation leaves the surface with buttons for an update it no longer knows.
	test("a check that finds nothing spares an operation in flight", () => {
		for (const phase of ["downloading", "ready", "applying"] as const) {
			const inFlight: UpdaterState = { ...ready, phase };
			expect(reduceUpdater(inFlight, { kind: "not_available" })).toBe(inFlight);
		}
	});

	// A dismissal is the user acting on that exact version, not a check answering late. It
	// overrides the sparing the not_available arm gives an operation in flight: a staged
	// update the user declined must stop being offered, and the backend deletes its staging.
	test("a dismissal clears the record it names, even one already staged", () => {
		const cleared = reduceUpdater(ready, { kind: "dismissed", version: "1.2.3" });

		expect(cleared.info).toBeNull();
	});

	// Choosing stable is not a way of asking for the dev build already found: the backend has
	// stopped any download of it and deleted its staging; no surface may keep offering it,
	// including the one that did not make the switch.
	test("a channel change clears the record whatever state it was in", () => {
		for (const phase of ["available", "downloading", "ready"] as const) {
			const before: UpdaterState = { ...ready, phase };
			expect(reduceUpdater(before, { kind: "channel_changed" }).info).toBeNull();
		}
	});

	// Same sparing as not_available, and for the same reason: this answers for the release
	// list, not for the download already running.
	test("a withheld notice spares an operation in flight", () => {
		const downloading: UpdaterState = { ...ready, phase: "downloading" };

		const withheld = reduceUpdater(downloading, { kind: "withheld", reason: "floor" });

		expect(withheld.info).toEqual(ready.info);
		expect(withheld.withheldReason).toBe("floor");
	});

	// The defect this hydration exists for: a surface mounting after the download began took
	// the phase and left the version behind. Both surfaces read the pair together; the
	// settings page showed "You're up to date." over a running download, and the toast went
	// down entirely, taking the only Cancel control with it.
	test("a download already running hydrates with the version it runs for", () => {
		const hydrated = hydrateUpdater(initialUpdaterState, {
			state: "Downloading",
			version: "2.0.0",
			last_info: { version: "2.0.0", download_size: 512 },
		});

		expect(hydrated.phase).toBe("downloading");
		expect(hydrated.info).toEqual({ version: "2.0.0", download_size: 512 });
	});

	// The other half of it: `Applying` matched no branch and fell through to the offer. An
	// apply already claimed came back as an offer with a live Download button under it.
	test("an apply already claimed hydrates as an apply, not as an offer", () => {
		const hydrated = hydrateUpdater(initialUpdaterState, {
			state: "Applying",
			version: "2.0.0",
			last_info: { version: "2.0.0", download_size: 512 },
		});

		expect(hydrated.phase).toBe("applying");
	});

	// A check that lands while a download runs replaces the offer with whatever the release
	// list now holds; the offer beside the phase can name another version entirely.
	test("an offer naming another version never repaints the operation", () => {
		const hydrated = hydrateUpdater(initialUpdaterState, {
			state: "Downloading",
			version: "2.0.0",
			last_info: { version: "3.0.0", download_size: 999 },
		});

		expect(hydrated.info).toEqual({ version: "2.0.0", download_size: 0 });
	});
});

describe("what a download request answers back", () => {
	// Keyed on the status code, never on the message: matching the string by hand is how
	// `applying` reached a user as the error text "applying", and how every code the match
	// did not happen to list became an error too.
	test("a refusal that names a phase is not an error the surface invents", () => {
		expect(eventForDownloadFailure(Object.assign(new Error("applying"), { code: 409 }))).toBeNull();
		expect(
			eventForDownloadFailure(Object.assign(new Error("download_in_progress"), { code: 409 })),
		).toBeNull();
	});
});
