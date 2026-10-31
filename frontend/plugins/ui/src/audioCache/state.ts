/** A reason is a constant phrase: no path or OS text reaches the page. */
export type CacheFailure = { action: "clear" | "resize"; reason: string };

const ACTIONS: readonly string[] = ["clear", "resize"];

/**
 * The backend's `CacheState`. `enabled` and `maxBytes` are what the cache applies now. `available`
 * is false when the index never opened: nothing is cached, whatever the switch says.
 */
export type CacheSnapshot = {
	seq: number;
	enabled: boolean;
	maxBytes: number;
	/** The cache's size as the backend's `format_size` prints it, here and in the menu. */
	usedText: string;
	available: boolean;
	lastFailure: CacheFailure | null;
};

export type AudioCacheState = {
	/** `null` until the backend answered. */
	snapshot: CacheSnapshot | null;
	/** Clicks the backend has not answered yet: the controls show them rather than spring back. */
	pendingEnabled: boolean | null;
	pendingGb: number | null;
	/** The latest click on each control. Only the answer to that one settles its pending. */
	enabledClick: number;
	gbClick: number;
};

export const initialAudioCacheState: AudioCacheState = {
	snapshot: null,
	pendingEnabled: null,
	pendingGb: null,
	enabledClick: 0,
	gbClick: 0,
};

export type Control = "enabled" | "gb";

/** How a click ended, by the backend's reply or by its refusal (which brings no snapshot). */
export type ClickOutcome = { reply: unknown } | "rejected";

/** The backend's `MIN_CAPACITY_GB`, `MAX_CAPACITY_GB` and `DEFAULT_CAPACITY_GB`. */
export const MIN_GB = 1;
export const MAX_GB = 6;
const DEFAULT_GB = 2;

const GIB = 1024 ** 3;

/** `undefined` for a failure this build cannot read, `null` for none. */
function parseFailure(raw: unknown): CacheFailure | null | undefined {
	if (raw === null || raw === undefined) return null;
	if (typeof raw !== "object") return undefined;
	const f = raw as Record<string, unknown>;
	if (typeof f.action !== "string" || !ACTIONS.includes(f.action) || typeof f.reason !== "string") return undefined;
	return { action: f.action as CacheFailure["action"], reason: f.reason };
}

function parseSnapshot(raw: unknown): CacheSnapshot | null {
	if (typeof raw !== "object" || raw === null) return null;
	const s = raw as Record<string, unknown>;
	const lastFailure = parseFailure(s.lastFailure);
	if (
		typeof s.seq !== "number" ||
		typeof s.enabled !== "boolean" ||
		typeof s.maxBytes !== "number" ||
		typeof s.usedText !== "string" ||
		typeof s.available !== "boolean" ||
		lastFailure === undefined
	) {
		return null;
	}
	return {
		seq: s.seq,
		enabled: s.enabled,
		maxBytes: s.maxBytes,
		usedText: s.usedText,
		available: s.available,
		lastFailure,
	};
}

function gbOf(snapshot: CacheSnapshot): number {
	return Math.round(snapshot.maxBytes / GIB);
}

/**
 * Take a snapshot from any path: a push, the pull, a click's reply. They leave from several backend
 * threads and can cross; one whose `seq` is not newer is stale. A snapshot never settles a click:
 * one taken before the click was applied can carry the very value it asked for.
 */
export function acceptSnapshot(state: AudioCacheState, incoming: unknown): AudioCacheState {
	const snapshot = parseSnapshot(incoming);
	if (!snapshot || (state.snapshot && snapshot.seq <= state.snapshot.seq)) return state;
	return { ...state, snapshot };
}

export function clickSwitch(state: AudioCacheState, on: boolean): AudioCacheState {
	return { ...state, pendingEnabled: on, enabledClick: state.enabledClick + 1 };
}

export function pickSize(state: AudioCacheState, gb: number): AudioCacheState {
	return { ...state, pendingGb: gb, gbClick: state.gbClick + 1 };
}

/**
 * The end of click number `click` on `control`. A reply's snapshot goes through the seq gate
 * whichever click it answers. The pending goes only if this was the control's latest click, even
 * when the gate refused the snapshot, the held one being no older than a reply taken after the change.
 */
export function settleClick(state: AudioCacheState, control: Control, click: number, outcome: ClickOutcome): AudioCacheState {
	const taken = outcome === "rejected" ? state : acceptSnapshot(state, outcome.reply);
	if (control === "enabled") {
		return click === taken.enabledClick && taken.pendingEnabled !== null ? { ...taken, pendingEnabled: null } : taken;
	}
	return click === taken.gbClick && taken.pendingGb !== null ? { ...taken, pendingGb: null } : taken;
}

/** The click first, then the backend's snapshot, then the backend's own default: on. */
export function switchPosition(state: AudioCacheState): boolean {
	return state.pendingEnabled ?? state.snapshot?.enabled ?? true;
}

export function sizePosition(state: AudioCacheState): number {
	return state.pendingGb ?? (state.snapshot ? gbOf(state.snapshot) : DEFAULT_GB);
}

/** What the cache holds against the cap it applies. `null` until the backend answered, or with no cache to hold anything. */
export function usageLine(state: AudioCacheState): string | null {
	const snapshot = state.snapshot;
	if (!snapshot || !snapshot.available) return null;
	return `${snapshot.usedText} of ${gbOf(snapshot)} GB used.`;
}

/** Whether the controls can do anything. True until the backend says the index never opened. */
export function cacheAvailable(state: AudioCacheState): boolean {
	return state.snapshot?.available ?? true;
}

/** The line under the Clear button. `null` where there is nothing to add. */
export function cacheNote(state: AudioCacheState): string | null {
	const snapshot = state.snapshot;
	if (!snapshot) return null;
	if (!snapshot.available) return "The cache could not be opened; nothing is cached.";
	const failure = snapshot.lastFailure;
	if (!failure) return null;
	switch (failure.action) {
		case "clear":
			return `Could not clear the cache: ${failure.reason}.`;
		case "resize":
			// The cap goes down before the eviction runs, which keeps the slider honest.
			return `The new size is kept, but the cache could not be trimmed to it: ${failure.reason}.`;
	}
}
