/** How a failure gets asked again: the backend's `Recovery`. */
export type Recovery = "timer" | "on_demand" | "never";

/** The backend's `PhaseView`. A reason is a constant phrase: no path or OS text reaches the page. */
export type CodecPhase =
	| { kind: "starting" }
	| { kind: "off" }
	| { kind: "unsupported" }
	| { kind: "downloading" }
	| { kind: "ready" }
	| { kind: "failed"; reason: string; recovery: Recovery };

/** The backend's `SnapshotView`. `enabled` is the switch as the backend holds it. */
export type CodecSnapshot = { seq: number; enabled: boolean; phase: CodecPhase };

export type VideoCodecState = {
	/** `null` until the backend answered. */
	snapshot: CodecSnapshot | null;
	/** The last click the backend has not echoed yet: the switch shows it rather than spring back. */
	pending: boolean | null;
};

export const initialVideoCodecState: VideoCodecState = { snapshot: null, pending: null };

const PLAIN_KINDS = new Set(["starting", "off", "unsupported", "downloading", "ready"]);
const RECOVERIES = new Set<string>(["timer", "on_demand", "never"]);

function parsePhase(raw: unknown): CodecPhase | null {
	if (typeof raw !== "object" || raw === null) return null;
	const phase = raw as Record<string, unknown>;
	if (typeof phase.kind !== "string") return null;
	if (PLAIN_KINDS.has(phase.kind)) return { kind: phase.kind } as CodecPhase;
	if (
		phase.kind === "failed" &&
		typeof phase.reason === "string" &&
		typeof phase.recovery === "string" &&
		RECOVERIES.has(phase.recovery)
	) {
		return { kind: "failed", reason: phase.reason, recovery: phase.recovery as Recovery };
	}
	return null;
}

function parseSnapshot(raw: unknown): CodecSnapshot | null {
	if (typeof raw !== "object" || raw === null) return null;
	const snapshot = raw as Record<string, unknown>;
	if (typeof snapshot.seq !== "number" || typeof snapshot.enabled !== "boolean") return null;
	const phase = parsePhase(snapshot.phase);
	return phase ? { seq: snapshot.seq, enabled: snapshot.enabled, phase } : null;
}

/**
 * Take a snapshot from either path, the push or the pull. They travel separately and can cross;
 * one whose `seq` is not newer is stale. A payload this build cannot read leaves the state as
 * it was: a `null` pull before the supervisor started, or a kind added later.
 */
export function acceptSnapshot(state: VideoCodecState, incoming: unknown): VideoCodecState {
	const snapshot = parseSnapshot(incoming);
	if (!snapshot || (state.snapshot && snapshot.seq <= state.snapshot.seq)) return state;
	const pending = state.pending === snapshot.enabled ? null : state.pending;
	return { snapshot, pending };
}

export function clickSwitch(state: VideoCodecState, on: boolean): VideoCodecState {
	return { ...state, pending: on };
}

/** The click first, then the backend's echo, then the backend's own default: on. */
export function switchPosition(state: VideoCodecState): boolean {
	return state.pending ?? state.snapshot?.enabled ?? true;
}

function sentence(phrase: string): string {
	return phrase.charAt(0).toUpperCase() + phrase.slice(1);
}

/** The line under the switch. `null` where the switch already says it all, or nothing is known. */
export function codecRowNote(state: VideoCodecState): string | null {
	const phase = state.snapshot?.phase;
	switch (phase?.kind) {
		case undefined:
		case "off":
			return null;
		case "starting":
			return "Checking the video codec...";
		case "downloading":
			return "Downloading Cisco's OpenH264 library...";
		case "ready":
			return "Ready.";
		case "unsupported":
			return "No video codec is published for this platform.";
		case "failed":
			switch (phase.recovery) {
				case "timer":
					return `${sentence(phase.reason)}. Retrying automatically.`;
				case "on_demand":
					return `${sentence(phase.reason)}. Will try again when needed, or turn the switch off and on to try now.`;
				case "never":
					// The switch is the one way back short of a relaunch: it probes again.
					return `${sentence(phase.reason)}. Turn the switch off and on to try again.`;
			}
	}
}
