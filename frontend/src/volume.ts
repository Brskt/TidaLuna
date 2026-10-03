import { invokeIpc, sendIpc } from "./ipc";

// What Rust reports of the level it plays (`LevelReport`, src/player/mod.rs): after a Connect
// controller or the OS mixer moved it, and to a page that loads.
export interface OutputLevelReport {
	volume: number;
	unmuteVolume: number;
	mute: boolean;
	// Someone chose this level: a page of any account, a controller, the OS mixer or a plugin.
	// The page takes it, and TIDAL's saved level restores nothing over it.
	chosen: boolean;
	// Bumped by every change, whoever made it, and by every page catching up.
	seq: number;
}

export function isOutputLevelReport(data: unknown): data is OutputLevelReport {
	const r = data as OutputLevelReport | null;
	return (
		typeof r?.volume === "number" &&
		typeof r.unmuteVolume === "number" &&
		typeof r.mute === "boolean" &&
		typeof r.chosen === "boolean" &&
		typeof r.seq === "number"
	);
}

// The one path a volume takes to Rust, whichever player TIDAL's SDK made active. TIDAL applies it
// to its active player only (the delegate's `setVolume`, or shaka's `mediaElement.volume`), and
// neither write carries the level: both are `desiredVolumeLevel` scaled by ReplayGain, and shaka
// also ramps crossfades and silences its idle element. A write only says "TIDAL applied a
// volume"; the level is the store's `playbackControls.volume` (0-100), where every volume action
// settles first.
class VolumeForwarder {
	#held: number | null = null;
	// The newest report of Rust's level this page has taken in. Every level sent carries it:
	// Rust drops one decided before the page caught up with its level or knew of a change a
	// controller or the OS mixer made, the store then holding a level older than Rust's.
	#seen = 0;

	constructor(private readonly send: (volume: number, seen: number) => void) {}

	// Sends `volume` unless Rust already holds it.
	forward(volume: number): void {
		if (volume === this.#held) return;
		this.resend(volume);
	}

	// Sends `volume` even if it was sent already: Rust may have refused it, the page not having
	// caught up yet when it left.
	resend(volume: number): void {
		this.#held = volume;
		this.send(volume, this.#seen);
	}

	// Rust reported this level itself. TIDAL re-applying it must not echo it back.
	adopt(volume: number): void {
		this.#held = volume;
	}

	// The page now knows Rust's level as of report `seq`.
	see(seq: number): void {
		if (seq > this.#seen) this.#seen = seq;
	}

	knows(seq: number): boolean {
		return seq <= this.#seen;
	}
}

const forwarder = new VolumeForwarder((volume, seen) => sendIpc("player.volume", volume, seen));

function storeVolume(): number | undefined {
	try {
		const { store } = require("../plugins/lib/src/redux/store");
		return store.getState()?.playbackControls?.volume;
	} catch {
		return undefined;
	}
}

// TIDAL applied a volume to its active player. `fallback` is the level the write itself carried,
// for a caller whose write is always user intent; shaka's element passes none, its writes being
// ramps and silences as often as intent.
export function applyVolume(fallback?: number): void {
	const volume = storeVolume() ?? fallback;
	if (volume !== undefined) forwarder.forward(volume);
}

export function adoptRustVolume(volume: number): void {
	forwarder.adopt(volume);
}

// Sends Rust each change of TIDAL's committed level, which is this app's output level.
//
// TIDAL's SDK applies a level only while its active player holds a stream (not before anything
// plays, nor after the queue ends). TIDAL still settles every local change (slider, keys, mute) in
// the store and saves it, and Rust plays on without that stream (a Connect cast, the next track):
// a change the SDK does not apply must still reach Rust, or it would play an older level. When
// the SDK does apply it, the forwarder drops the repeat. Rust's own reports are adopted before
// they are dispatched, and never echo back.
export function followCommittedVolume(store: {
	getState(): any;
	subscribe(cb: () => void): () => void;
}): () => void {
	const committed = (): unknown => store.getState()?.playbackControls?.volume;
	let last = committed();
	return store.subscribe(() => {
		const volume = committed();
		if (volume === last) return;
		last = volume;
		if (typeof volume === "number") forwarder.forward(volume);
	});
}

type Store = { dispatch: (action: { type: string; payload?: unknown }) => void };

// Writes a level Rust applied into TIDAL's store, as state and never as SET_MUTE: TIDAL's saga
// for that one applies the store's own level to restore, while Rust has already restored its own.
// SET_VOLUME goes last, so the triple TIDAL persists on it is the settled one. A report older than
// one already taken in changes nothing.
export function mirrorRustLevel(store: Store, report: OutputLevelReport): void {
	if (forwarder.knows(report.seq)) return;
	const volume = Math.round(report.volume);
	adoptRustVolume(volume);
	store.dispatch({
		type: "playbackControls/SET_VOLUME_UNMUTE",
		payload: { volume: Math.round(report.unmuteVolume) },
	});
	store.dispatch({
		type: "playbackControlsActions/INDICATE_MUTE",
		payload: { mute: report.mute },
	});
	store.dispatch({
		type: "playbackControls/SET_VOLUME",
		payload: { volume },
	});
	// Taken in only once the store holds it: a store that threw still shows the older level, and
	// a level applied from it must not reach Rust as if the page knew better.
	forwarder.see(report.seq);
}

// A page that loads catches up with the level Rust plays, once TIDAL's store exists. This install
// plays one level whichever account loads: once someone chose it, Rust's is mirrored, and
// `__lunaRestoreVolume` (volume_restore.js) kept TIDAL from restoring this account's own over it.
// Until then TIDAL's saved level seeds it: the store's settled level goes to Rust, which may have
// refused what this page sent before catching up.
export function catchUpWithRust(store: Store & { getState(): any }, report: unknown): void {
	if (!isOutputLevelReport(report)) return;
	if (report.chosen) {
		mirrorRustLevel(store, report);
		return;
	}
	forwarder.see(report.seq);
	const volume = store.getState()?.playbackControls?.volume;
	if (typeof volume === "number") forwarder.resend(volume);
}

export async function resyncOutputLevel(store: Store & { getState(): any }): Promise<void> {
	catchUpWithRust(store, await invokeIpc("connect.catch_up_level"));
}
