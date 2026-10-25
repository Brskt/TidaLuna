// The player SDK's API, to unload the native player's preload without TIDAL's `Bat` saga, whose
// `put(PRELOAD_SUCCESS)` also releases the fallback `Wat` keeps. Captured by the Rust filter from
// the instance TIDAL runs on (`module_capture.rs`), found by shape: the minified name changes.

export type SdkPlayer = { setNext(product?: unknown): Promise<unknown> };

// The module id the Rust filter captures it under (`SDK_PLAYER_API`, `module_capture.rs`).
export const SDK_PLAYER_API = "@tidal-music/player";

const isSdkPlayer = (o: unknown): o is SdkPlayer => {
	const api = o as Record<string, unknown> | null;
	return typeof api === "object" && api !== null && ["setNext", "load", "reset", "setEventSender"].every((k) => typeof api[k] === "function");
};

export const findSdkPlayer = (host: Record<string, unknown> | undefined = (globalThis as { __lunaHostModules?: Record<string, unknown> }).__lunaHostModules): SdkPlayer | null => {
	const captured = host?.[SDK_PLAYER_API];
	if (typeof captured !== "object" || captured === null) return null;
	return Object.values(captured).find(isSdkPlayer) ?? null;
};

let sdk: SdkPlayer | null = null;

/** The SDK's API, looked for again until it is found (a chunk can evaluate after the first look). */
export const sdkPlayer = (): SdkPlayer | null => (sdk ??= findSdkPlayer());

/**
 * Looks for the SDK's API from `init`, after TIDAL's app is mounted and its chunks evaluated.
 * A miss is said once; `sdkPlayer()` keeps looking, and the callers go without until it is found.
 */
export function startSdkPlayer(find: () => SdkPlayer | null = findSdkPlayer): void {
	sdk = find();
	if (sdk === null) console.warn("[luna:sdk-player] the SDK's API was not captured: a next left behind by a change of player is not unloaded");
}
