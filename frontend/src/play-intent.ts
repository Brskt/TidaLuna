// Whose the listener's Play is to start. From a stopped player TIDAL's PLAY saga reloads the track
// and calls the delegate's `play()` itself; a Play sent ahead of that reload starts the engine,
// which the reload then stops and starts again.

export type PlayRoute = "now" | "sdk";

export interface PlayFacts {
	/** `playbackControls`: the saga reloads from `IDLE` with nothing prefilled. */
	controls?: { playbackState?: unknown; prefilled?: unknown };
	/** TIDAL's own reachability: offline its reload fails before any load reaches Rust. */
	network?: { hasNetworkConnection?: unknown };
	queue?: { elements?: { mediaItemId?: unknown }[]; currentIndex?: unknown };
	mediaItems?: Record<string, unknown>;
	/** Rust refused the SDK's last load, and the delegate drops the play of it. */
	loadRefused: boolean;
	/** A cast owns the engine: a local Play resumes it, and nothing loads. */
	castShown: boolean;
}

export function playRoute(facts: PlayFacts): PlayRoute {
	const { controls, queue } = facts;
	const reloads = controls?.playbackState === "IDLE" && controls.prefilled !== true;
	const online = facts.network?.hasNetworkConnection !== false;
	// The saga's own reading of the current track (`OQ`): the queue entry, then its catalog item.
	const entry = typeof queue?.currentIndex === "number" ? queue.elements?.[queue.currentIndex] : undefined;
	const queued = entry != null && Boolean(facts.mediaItems?.[String(entry.mediaItemId)]);
	return reloads && online && queued && !facts.loadRefused && !facts.castShown ? "sdk" : "now";
}
