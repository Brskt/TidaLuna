// TIDAL's loudness normalization, computed by the page and applied by Rust. The SDK folds the
// factor into the volume it hands its native player; ours keeps the listener's level apart (Rust,
// the OS mixer and Connect share it): each track's factors travel with the track instead.

// `normalize-volume.ts` of `@tidal-music/player`, which it does not export: peak fixed at 1,
// pre-amp +4 dB. It only attenuates.
const PRE_AMP_DB = 4;
export const normalizeVolume = (replayGain: number): number => Math.min(10 ** ((PRE_AMP_DB + replayGain) / 20), 1);

export type ReplayGains = { album?: number; track?: number };

// `[album, track]`, the order Rust reads them in. A gain applies only when it is truthy
// (`adjustedVolume`); an absent or zero gain plays the track untouched.
export const factorsOf = (gains: ReplayGains | undefined): [number, number] => [
	gains?.album ? normalizeVolume(gains.album) : 1,
	gains?.track ? normalizeVolume(gains.track) : 1,
];

type Store = { getState(): any; subscribe(cb: () => void): () => void };

// The store's "Normalize volume" setting (`settings.audioNormalization`: NONE, ALBUM or TRACK),
// the one TIDAL's saga hands the SDK. Sent at start and on every change: the volume re-applies
// on a toggle, and Rust then follows on the playing track's next buffer.
export function followNormalization(store: Store, send: (mode: unknown) => void): () => void {
	const mode = (): unknown => store.getState()?.settings?.audioNormalization;
	let last = mode();
	send(last);
	return store.subscribe(() => {
		const now = mode();
		if (now === last) return;
		last = now;
		send(now);
	});
}
