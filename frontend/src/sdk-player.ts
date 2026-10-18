// The player SDK's own API (`setNext`, `load`, `reset`, ...), for the one call TIDAL's actions cannot make:
// unloading the native player's preload without TIDAL's `Bat` saga, whose `put(PRELOAD_SUCCESS)` also
// releases the fallback TIDAL's `Wat` keeps for a preload that did not finish.
//
// The Rust filter captures the exports of the chunk that defines it as the page's own modules evaluate
// that chunk (`module_capture.rs`, `capture_sdk_api`). This is the instance TIDAL's code runs on, with
// no second evaluation of it. The API is the one export that carries the functions below under their own
// names (the namespace TIDAL's entry chunk imports as `sdk`): found by that shape, not by the minified
// name of its export, which changes with every build.

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
