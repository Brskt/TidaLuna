// Rust's transport state, told to Luna's interceptors (blind to the SDK's own write, a saga put) and
// written into TIDAL's store during a cast only. Elsewhere TIDAL alone writes it, as in the
// official app: its silence after a reset keeps the IDLE that makes the next Play reload.

import { deliver } from "../render/src/exposeTidalInternals.patchAction";

const TYPE = "playbackControls/SET_PLAYBACK_STATE";

// The value Luna's interceptors are told for each Rust state.
const NOTIFIED: Record<string, string> = {
	active: "PLAYING",
	paused: "NOT_PLAYING",
	stopped: "NOT_PLAYING",
	completed: "IDLE",
	idle: "IDLE",
};

export type StateWrite = { action: { type: string; payload: string }; before: unknown; mayStore: boolean };

// Read before anything hears the event: the self-load's element events cause a write too.
export function beginStateWrite(rustState: unknown, cast: boolean, readState: () => unknown): StateWrite | undefined {
	const payload = NOTIFIED[rustState as string];
	if (!payload) return undefined;
	return {
		action: { type: TYPE, payload },
		before: readState(),
		// A cast's tracks are no session, and nothing else writes then.
		mayStore: cast,
	};
}

// After the SDK heard the event. A store it moved, or one already holding the value, takes no write
// of ours: TIDAL's play-time reducer counts every write since the last PLAYING.
export function settleStateWrite(write: StateWrite, readState: () => unknown, dispatch: (action: StateWrite["action"]) => void): void {
	const after = readState();
	const storeHasIt = after !== write.before || after === write.action.payload;
	if (write.mayStore && !storeHasIt) dispatch(write.action);
	else deliver(write.action);
}
