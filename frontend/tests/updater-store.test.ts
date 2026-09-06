import { describe, expect, test } from "bun:test";

import {
	dismissKey,
	eventForDownloadFailure,
	eventForDownloadReply,
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
	// The defect this file exists for: the toast listened to available/ready/error/cancelled
	// and never to applying. An apply started from the settings page left it offering a
	// restart the backend had already refused. One reducer means a surface cannot carry a
	// subset of the transitions: it either has the record or it does not.
	test("applying is a transition of the shared record, not of one surface", () => {
		expect(reduceUpdater(ready, { kind: "applying" }).phase).toBe("applying");
	});

	test("a ready fills a missing record without overwriting a richer one", () => {
		const fromNothing = reduceUpdater(initialUpdaterState, { kind: "ready", version: "9.9.9" });
		expect(fromNothing.info).toEqual({ version: "9.9.9", download_size: 0 });

		// A ready that follows a check must not drop the size the check reported.
		const overReady = reduceUpdater(ready, { kind: "ready", version: "1.2.3" });
		expect(overReady.info).toEqual({ version: "1.2.3", download_size: 4096 });
	});

	// The two surfaces disagreed here too: one reset to "available" on any cancel, the other
	// only from "downloading". Against an apply already claimed, the loose reading offers the
	// update again while the app is on its way out.
	test("a cancel only undoes a download", () => {
		const downloading: UpdaterState = { ...ready, phase: "downloading" };
		expect(reduceUpdater(downloading, { kind: "cancelled" }).phase).toBe("available");

		const applying: UpdaterState = { ...ready, phase: "applying" };
		expect(reduceUpdater(applying, { kind: "cancelled" }).phase).toBe("applying");
	});

	// The check button dropped a "nothing available" answer on the floor: the branch that
	// pushed an offer had no counterpart; the record survived every check that disproved
	// it. "You're up to date." became unreachable for the rest of the session, and the toast
	// kept a Download button pointed at the ruled-out version.
	test("a check that finds nothing takes back the record it disproves", () => {
		const offered: UpdaterState = { ...ready, phase: "available" };

		const cleared = reduceUpdater(offered, { kind: "not_available" });

		expect(cleared.info).toBeNull();
		expect(cleared.errorMsg).toBe("");
	});

	// Same rule as the cancel above, for the same reason: a check answers while a download or
	// an apply may already be acting on the offer, and taking the record back mid-operation
	// leaves the surface with buttons for an update it no longer knows.
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

	test("a dismissal leaves a record naming another version", () => {
		expect(reduceUpdater(ready, { kind: "dismissed", version: "9.9.9" })).toBe(ready);
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

	// The automatic check could announce one outcome out of four: an update it found. A newer
	// version a gate refuses (the migration floor, or a Linux bootstrap too old to take it)
	// shouted into a channel with no listener, and the log line that carried it is gated behind
	// a level nobody runs. The reason has to land in the record both surfaces read, and it must
	// not arrive as an offer: the version exists and is precisely what may not be installed.
	test("a withheld update is reported without being offered", () => {
		const offered: UpdaterState = { ...ready, phase: "available" };

		const withheld = reduceUpdater(offered, { kind: "withheld", reason: "bootstrap behind" });

		expect(withheld.info).toBeNull();
		expect(withheld.withheldReason).toBe("bootstrap behind");
	});

	test("an offer supersedes the withheld notice it answers", () => {
		const withheld = reduceUpdater(ready, { kind: "withheld", reason: "bootstrap behind" });

		const offered = reduceUpdater(withheld, {
			kind: "available",
			info: { version: "2.0.0", download_size: 1 },
		});

		expect(offered.withheldReason).toBe("");
	});

	// Same sparing as not_available, and for the same reason: this answers for the release
	// list, not for the download already running.
	test("a withheld notice spares an operation in flight", () => {
		const downloading: UpdaterState = { ...ready, phase: "downloading" };

		const withheld = reduceUpdater(downloading, { kind: "withheld", reason: "floor" });

		expect(withheld.info).toEqual(ready.info);
		expect(withheld.withheldReason).toBe("floor");
	});

	test("an error keeps its message with the phase that reports it", () => {
		const failed = reduceUpdater(ready, { kind: "error", message: "disk full" });
		expect(failed.phase).toBe("error");
		expect(failed.errorMsg).toBe("disk full");
	});

	test("an error with no message of its own still carries one", () => {
		// The fallback belongs to the producer. Both surfaces used to carry their own copy of
		// it, which is one reader away from an error line rendering blank.
		expect(reduceUpdater(ready, { kind: "error", message: "" }).errorMsg).toBe("Download failed");
	});

	// A phase that names an operation names the version it acts on: an event arriving with
	// no offer held is not a transition. The old record could hold the phase alone, and both
	// surfaces then had to guess what it meant.
	test("an operation with no offer held is not a transition", () => {
		expect(reduceUpdater(initialUpdaterState, { kind: "downloading" })).toBe(initialUpdaterState);
		expect(reduceUpdater(initialUpdaterState, { kind: "applying" })).toBe(initialUpdaterState);
	});

	test("the status reply hydrates the same record the events feed", () => {
		expect(hydrateUpdater(initialUpdaterState, { state: "Ready", version: "2.0.0" })).toEqual({
			info: { version: "2.0.0", download_size: 0 },
			phase: "ready",
			errorMsg: "",
			withheldReason: "",
		});
		// Nothing known yet is not a transition: an empty status must leave the record alone.
		expect(hydrateUpdater(ready, {})).toBe(ready);
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

	test("a phase the backend names without a version is not painted", () => {
		expect(hydrateUpdater(initialUpdaterState, { state: "Downloading" })).toBe(
			initialUpdaterState,
		);
	});

	test("an idle backend still holding an offer hydrates it as one", () => {
		const hydrated = hydrateUpdater(initialUpdaterState, {
			state: "Idle",
			last_info: { version: "2.0.0", download_size: 512 },
		});

		expect(hydrated.phase).toBe("available");
		expect(hydrated.info).toEqual({ version: "2.0.0", download_size: 512 });
	});
});

describe("what a download request answers back", () => {
	// The defect this block exists for: a surface paints `downloading` on the click, because
	// the backend announces no download that starts. The toast never read the reply. An
	// update already staged left it on "Downloading update..." with a dead Cancel button for
	// the rest of the session, while the settings page (reading the same shared record)
	// healed. One mapping is what stops two surfaces deriving the phase differently.
	test("a version already staged answers with the phase, not with silence", () => {
		expect(eventForDownloadReply("already_ready", "1.2.3")).toEqual({
			kind: "ready",
			version: "1.2.3",
		});
	});

	test("a download that starts leaves the paint the surface already made", () => {
		expect(eventForDownloadReply("started", "1.2.3")).toBeNull();
	});

	// Keyed on the status code, never on the message: matching the string by hand is how
	// `applying` reached a user as the error text "applying", and how every code the match
	// did not happen to list became an error too.
	test("a refusal that names a phase is not an error the surface invents", () => {
		expect(eventForDownloadFailure(Object.assign(new Error("applying"), { code: 409 }))).toBeNull();
		expect(
			eventForDownloadFailure(Object.assign(new Error("download_in_progress"), { code: 409 })),
		).toBeNull();
	});

	test("a refusal that names no phase is reported as it came", () => {
		expect(
			eventForDownloadFailure(Object.assign(new Error("not the current offer"), { code: 403 })),
		).toEqual({ kind: "error", message: "not the current offer" });
	});

	// Whatever threw did not come through the IPC bridge: it carries no code and no
	// message worth showing. Reporting nothing at all is what left the surface painted.
	test("a failure that says nothing is still reported", () => {
		expect(eventForDownloadFailure(undefined)).toEqual({
			kind: "error",
			message: "Download failed",
		});
	});
});

describe("what a surface dismisses", () => {
	const notInstalled = {
		phase: "error",
		info: null,
		errorMsg: "Updater is not installed",
		withheldReason: "",
	} satisfies UpdaterState;

	// A failure is reported whether or not an offer was ever known, and the record the user is
	// looking at may hold no version at all. Keyed on the version alone, the dismissal of such a
	// record stored the absence itself: a surface holding "nothing dismissed" as the same
	// absence could never match it. Its Close button did nothing for as long as the failure
	// stood.
	test("a failure with no offer still has an identity to dismiss", () => {
		expect(dismissKey(notInstalled).length).toBeGreaterThan(0);
	});

	// Two failures that say different things are two different things to acknowledge: closing
	// the first must not take the second down with it, unseen.
	test("one failure is not dismissed by the acknowledgement of another", () => {
		const refused = { ...notInstalled, errorMsg: "That update is no longer the one staged" };
		expect(dismissKey(refused)).not.toBe(dismissKey(notInstalled));
	});

	// The version is the identity wherever one exists, and it spans the phases an offer moves
	// through: a download that becomes ready is the same release, re-announced, and a toast the
	// user already put down stays down. Only the next release raises it again.
	test("an offer is its version, whatever phase it has reached", () => {
		expect(dismissKey({ ...ready, phase: "downloading" })).toBe(dismissKey(ready));
	});
});
