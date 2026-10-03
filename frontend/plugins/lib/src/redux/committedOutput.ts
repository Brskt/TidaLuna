import type { PlayerDeviceMode, TidalStoreState } from "./types/store";

export interface CommittedOutput {
	id: string;
	mode: PlayerDeviceMode;
}

/**
 * The output device TIDAL's store says is active, under Rust's id and in the store's mode (the
 * store commits a device once the SDK applied it). The system default is `"auto"`, as
 * `selectSystemDevice` sends it, and always shared: TIDAL's native player asks for it with no
 * mode. Null for a Chromium-only device: no `nativeDeviceId`, and Rust cannot open it.
 */
export const committedOutput = (state: TidalStoreState): CommittedOutput | null => {
	const player = state?.player;
	const device = player?.availableDevices?.find((d) => d.id === player.activeDeviceId);
	const nativeId: string | undefined = device?.nativeDeviceId;
	if (!nativeId) return null;
	if (nativeId === "default") return { id: "auto", mode: "shared" };
	return { id: nativeId, mode: player.activeDeviceMode === "exclusive" ? "exclusive" : "shared" };
};
