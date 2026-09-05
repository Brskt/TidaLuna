import type { UpdateInfo } from "../types/updater";

/**
 * What the backend owns about an update, as the renderer knows it.
 *
 * One record for every surface. Two components each holding their own copy is what let the
 * toast go on offering "Apply & Restart" while an apply started from the settings page was
 * already in flight: it listened to four of the five events and never learned about the
 * fifth. A surface added later inherits the whole record instead of a fresh subset of it.
 *
 * The version lives INSIDE the phase that acts on it, as it does in the backend's own
 * `UpdaterPhase` enum. Held apart (a phase string beside a nullable record), the renderer
 * could express what the backend cannot: an operation on a version it does not name. Both
 * surfaces then guessed at it, and guessed differently. The settings page collapsed such a
 * state to "You're up to date." while a download was running, and the toast rendered nothing
 * at all; the only Cancel control went with it.
 *
 * `errorMsg` and `withheldReason` stay siblings of the union: a withheld notice lands while
 * an operation is in flight, and belongs to no single phase.
 *
 * Kept apart from the store that feeds it, for the transitions to be tested without an IPC
 * bridge: this file imports nothing but types.
 */
export type UpdaterRecord =
	| { phase: "idle"; info: null }
	| { phase: "available"; info: UpdateInfo }
	| { phase: "downloading"; info: UpdateInfo }
	| { phase: "ready"; info: UpdateInfo }
	| { phase: "applying"; info: UpdateInfo }
	/** A failure is reported whether or not an offer was ever known, so this one may hold none. */
	| { phase: "error"; info: UpdateInfo | null };

export type UpdaterState = UpdaterRecord & {
	errorMsg: string;
	/**
	 * Why a newer version that does exist is not on offer here: a migration floor, or a
	 * Linux bootstrap too old to take it. Kept apart from `errorMsg`: nothing failed, and a
	 * surface must report it without ever offering the version it names.
	 */
	withheldReason: string;
};

/** The phase vocabulary, derived so it cannot drift from the record that defines it. */
export type UpdaterPhase = UpdaterRecord["phase"];

/** The updater events, named once rather than spelled out at each listener. */
export type UpdaterEvent =
	| { kind: "available"; info: UpdateInfo }
	| { kind: "ready"; version: string }
	| { kind: "downloading" }
	| { kind: "applying" }
	| { kind: "error"; message: string }
	| { kind: "cancelled" }
	| { kind: "not_available" }
	| { kind: "dismissed"; version: string }
	| { kind: "withheld"; reason: string }
	| { kind: "channel_changed" };

export const initialUpdaterState: UpdaterState = {
	phase: "idle",
	info: null,
	errorMsg: "",
	withheldReason: "",
};

/**
 * What a surface puts down when it dismisses the record it is showing.
 *
 * The version wherever one exists, which keeps a release already put down from rising across the
 * phases it moves through; only the next release raises a surface again. The `error` variant may
 * hold no offer at all, and an absence is not an identity: keyed on it, a dismissal is
 * indistinguishable from having dismissed nothing. A surface could never match its own gesture,
 * and its Close did nothing for as long as the failure stood. A failure with no offer is named
 * by what it says instead.
 */
export function dismissKey(state: UpdaterState): string {
	return state.info ? `v:${state.info.version}` : `e:${state.errorMsg}`;
}

/** The next state an event implies. */
export function reduceUpdater(state: UpdaterState, event: UpdaterEvent): UpdaterState {
	switch (event.kind) {
		case "available":
			// An offer answers a refusal that came before it: the gate that withheld the
			// previous candidate has nothing to say about this one.
			return { ...state, info: event.info, phase: "available", withheldReason: "" };
		case "withheld": {
			// The version exists and is exactly what may not be installed. The reason is
			// recorded and no offer is. In flight is spared for the reason `not_available`
			// spares it: this answers for the release list, not for the running download.
			const inFlight =
				state.phase === "downloading" || state.phase === "ready" || state.phase === "applying";
			return inFlight
				? { ...state, withheldReason: event.reason }
				: { ...initialUpdaterState, withheldReason: event.reason };
		}
		case "ready":
			// The version only fills a gap. A ready that follows a check already carries the
			// richer record, and overwriting it drops the download size with it.
			return {
				...state,
				info: state.info ?? { version: event.version, download_size: 0 },
				phase: "ready",
			};
		case "downloading":
			// A download acts on an offer; without one there is nothing to paint. The type
			// asks the question the old record let this arm skip: the surfaces both gate their
			// click on an offer, and this is what says so where every caller is held to it.
			return state.info ? { ...state, phase: "downloading", info: state.info } : state;
		case "applying":
			return state.info ? { ...state, phase: "applying", info: state.info } : state;
		case "error":
			// The message is the whole point of this phase: its fallback belongs to the
			// producer. It lived in both surfaces instead, as the same `|| "Download failed"`
			// twice, which is one reader away from an error line rendering empty.
			return { ...state, errorMsg: event.message || "Download failed", phase: "error" };
		case "cancelled":
			// Only a download can be cancelled. Any other phase keeps its own: a cancel
			// arriving against an apply already claimed would otherwise offer the update
			// again while the backend is on its way out.
			return state.phase === "downloading"
				? { ...state, phase: "available", info: state.info }
				: state;
		case "channel_changed":
			// Everything the record held was resolved from the channel that just changed, and
			// the backend has already stopped any download of it and deleted its staging. No
			// phase is spared here: there is nothing left in flight to spare.
			return initialUpdaterState;
		case "dismissed":
			// The user acting on that exact version, not a check answering late about the
			// release list. Unlike `not_available`, this does not spare an operation in
			// flight: the backend has already deleted the staging of a declined update, and
			// a surface still offering a restart for it would be offering nothing.
			return state.info?.version === event.version ? initialUpdaterState : state;
		case "not_available": {
			// A check that reached an answer supersedes the record it disproves, and this
			// record is where both surfaces read whether an update exists at all; the
			// answer has to land here, not in the surface that asked for it.
			//
			// The in-flight phases are spared for the reason the cancel above is narrow: a
			// download, a staged update and an apply each act on the offer they were started
			// with, and a check speaks for the release list rather than for that operation.
			const inFlight =
				state.phase === "downloading" || state.phase === "ready" || state.phase === "applying";
			return inFlight ? state : initialUpdaterState;
		}
	}
}

/**
 * What a `updater.download` reply implies for the record, or `null` when it implies nothing.
 *
 * A surface paints `downloading` on the click, because the backend announces no download that
 * starts. Every other outcome has to undo that paint, and the reply is the only place the
 * asker learns of one: the mapping lives here, once, rather than in each surface. It lived
 * in two of them: one read `already_ready` and healed, the other read nothing and stayed on
 * "Downloading update..." for the rest of the session.
 *
 * Kept as a belt beside the backend's own announcement rather than instead of it: a broadcast
 * runs a script on a frame that may be gone, and it says so to nobody.
 */
export function eventForDownloadReply(reply: unknown, version: string): UpdaterEvent | null {
	return reply === "already_ready" ? { kind: "ready", version } : null;
}

/**
 * What a failed `updater.download` implies for the record, or `null` when the backend has
 * already announced it.
 *
 * A refusal that names a phase is not a failure: the backend broadcasts that phase, and a
 * surface inventing an `error` for it paints a failure over a state that is fine. Keyed on the
 * status code rather than the message, because the message is a protocol string. Matching it
 * by hand is how `applying` reached a user as the error text "applying", and how every code
 * the match did not list became one too.
 */
export function eventForDownloadFailure(err: unknown): UpdaterEvent | null {
	const failure = typeof err === "object" && err !== null ? (err as Record<string, unknown>) : {};
	if (failure.code === 409) return null;
	const message = typeof err === "string" ? err : (failure.message as string) || "";
	return { kind: "error", message: message || "Download failed" };
}

/**
 * The tags `updater.status` puts on the wire, which are NOT the phase vocabulary above.
 *
 * They are the variants of `UpdaterPhase` in `src/updater/types.rs`, adjacently tagged and
 * flattened into the reply beside `last_info`. Named here because the reply crosses the
 * bridge untyped: a tag no branch listed used to fall through in silence, which is how
 * `Applying` reached both surfaces painted as an offer with a live Download button.
 */
type UpdaterStatusTag = "Idle" | "Downloading" | "Ready" | "Applying";

interface UpdaterStatusReply {
	state?: UpdaterStatusTag;
	version?: string | null;
	last_info?: UpdateInfo | null;
}

/** The phase each in-flight tag names, so the translation states its mapping once. */
const PHASE_FOR_TAG = {
	Downloading: "downloading",
	Ready: "ready",
	Applying: "applying",
} as const;

/**
 * The record an operation on `version` deserves.
 *
 * The version comes from the phase and never from the offer beside it: a check that lands
 * while an operation runs replaces `last_info` with whatever the release list now holds, and
 * it can name a different version than the one being downloaded. Either record is taken only
 * when it names this version, because a bare tag carries no download size and a surface that
 * mounted mid-operation holds none of its own.
 */
function infoFor(state: UpdaterState, offer: UpdateInfo | null, version: string): UpdateInfo {
	if (state.info?.version === version) return state.info;
	if (offer?.version === version) return offer;
	return { version, download_size: 0 };
}

/** The status reply, which hydrates the shared record rather than each surface separately. */
export function hydrateUpdater(state: UpdaterState, status: unknown): UpdaterState {
	const reply: UpdaterStatusReply =
		typeof status === "object" && status !== null ? (status as UpdaterStatusReply) : {};
	const offer = reply.last_info ?? null;
	switch (reply.state) {
		case "Downloading":
		case "Ready":
		case "Applying":
			// An operation the backend names without a version is a reply this build cannot
			// act on, and painting the phase without one is the state the record forbids.
			return reply.version
				? {
						...state,
						phase: PHASE_FOR_TAG[reply.state],
						info: infoFor(state, offer, reply.version),
					}
				: state;
		case "Idle":
		case undefined:
			// An offer with nothing acting on it, or a reply naming no phase at all: the
			// record only learns what the backend still has to offer. Nothing known yet is
			// not a transition: an empty status leaves the record alone.
			return offer ? { ...state, phase: "available", info: offer } : state;
		default: {
			// A tag this build does not know. `updater.status` is the only place a surface
			// learns of an operation that started before it mounted; a silent fall-through
			// here loses that operation entirely, the defect this switch replaces.
			const unhandled: never = reply.state;
			console.warn(`[updater] status reply names an unknown phase: ${String(unhandled)}`);
			return state;
		}
	}
}
