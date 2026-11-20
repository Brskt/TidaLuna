// The player SDK's API and session singleton, captured by the Rust filter from the instance TIDAL
// runs on (`module_capture.rs`) and found by shape. The API unloads the native player's preload
// without TIDAL's `Bat` saga, whose `put(PRELOAD_SUCCESS)` also releases the fallback `Wat` keeps.

export type SdkPlayer = { setNext(product?: unknown): Promise<unknown> };

// The module id the Rust filter captures it under (`SDK_PLAYER_API`, `module_capture.rs`).
export const SDK_PLAYER_API = "@tidal-music/player";

const isSdkPlayer = (o: unknown): o is SdkPlayer => {
	const api = o as Record<string, unknown> | null;
	return typeof api === "object" && api !== null && ["setNext", "load", "reset", "setEventSender"].every((k) => typeof api[k] === "function");
};

type Host = Record<string, unknown> | undefined;
const capturedHost = (): Host => (globalThis as { __lunaHostModules?: Record<string, unknown> }).__lunaHostModules;

const findCaptured = <T>(host: Host, is: (o: unknown) => o is T): T | null => {
	const captured = host?.[SDK_PLAYER_API];
	if (typeof captured !== "object" || captured === null) return null;
	return Object.values(captured).find(is) ?? null;
};

export const findSdkPlayer = (host: Host = capturedHost()): SdkPlayer | null => findCaptured(host, isSdkPlayer);

// The SDK's session singleton, exported by the same chunk: `activePlayer` is the player it hands a
// device change to, undefined after its reset. Matched by its fields and getters, whose names the
// minifier keeps.
type SdkSession = { activePlayer?: { name?: unknown } };

// Taken before any plugin loads.
const hasOwn = Object.hasOwn;

// Its two fields are own properties: `in` alone would also read names a plugin left on
// Object.prototype, and take any export for the session. The getter can live on the prototype.
const isSdkSession = (o: unknown): o is SdkSession =>
	typeof o === "object" && o !== null && hasOwn(o, "activePlayer") && hasOwn(o, "preloadPlayer") && "preloadedMediaProduct" in o;

/**
 * Whether the SDK's active player is its native one, by TIDAL's own test (`name === "nativePlayer"`),
 * or null while the session is not captured. Read live: TIDAL switches players in its own sagas.
 */
export const sdkNativeActive = (host: Host = capturedHost()): boolean | null => {
	const session = findCaptured(host, isSdkSession);
	return session === null ? null : session.activePlayer?.name === "nativePlayer";
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
