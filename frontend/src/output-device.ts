import { sendIpc } from "./ipc";
import { isSelfLoad } from "./audio-proxy";
import { type CommittedOutput, committedOutput } from "../plugins/lib/src/redux/committedOutput";

// The output device Rust plays on, whichever player TIDAL's SDK made active. TIDAL hands a device
// choice to its active player only (`set activeDevice` -> `updateOutputDevice()`): the delegate's
// `selectDevice` on a native track, shaka's `setSinkId` on a silent element for a self-loaded
// one, which Rust never hears. Both commit the device to the store once applied
// (`player.activeDeviceId`, mode reset to shared), and a self-loaded track follows that.

export type DeviceMode = "shared" | "exclusive";

const W = window as any;

// ASIO has no TIDAL device mode and survives a "shared" re-assert (a track change). An explicit
// "exclusive" is the user switching modes: it clears the flag, the two being exclusive of each
// other.
export function selectDevice(id: string, mode: DeviceMode): void {
	let effective: string;
	if (mode === "exclusive") {
		if (W.__TIDALUNAR_ASIO__ === true) {
			W.__TIDALUNAR_ASIO__ = false;
			sendIpc("settings.asio", false);
		}
		effective = "exclusive";
	} else {
		effective = W.__TIDALUNAR_ASIO__ === true ? "asio" : "shared";
	}
	sendIpc("player.devices.set", id, effective);
}

// The module id the Rust filter captures the SDK's `output-devices-*` chunk under
// (`module_capture.rs`): its one export is the `outputDevices` singleton the SDK decides from.
export const SDK_OUTPUT_DEVICES = "@tidal-music/player/output-devices";

let warnedUncaptured = false;

// The device the SDK still holds when it asks its native player for the system device, or null
// when that is the system device. `updateOutputDevice` asks only while the default is active;
// `abandon()` asks on every hand-off to shaka in exclusive mode, whatever the device, to release
// the lock. A held non-default device is therefore that release. The store cannot tell: on a
// System Default pick it still names the old device until the SDK confirms the switch.
export function deviceReleasedToShaka(): string | null {
	const sdk = W.__lunaHostModules?.[SDK_OUTPUT_DEVICES]?.outputDevices;
	if (!sdk) {
		if (!warnedUncaptured) {
			warnedUncaptured = true;
			console.warn("[luna] SDK outputDevices not captured: a hand-off to shaka moves playback onto the default device");
		}
		return null;
	}
	const held: unknown = sdk.activeDevice?.nativeDeviceId;
	return typeof held === "string" && held !== "default" ? held : null;
}

const keyOf = (out: CommittedOutput | null) => (out ? `${out.mode}:${out.id}` : null);

// Sends Rust each change of the committed device made while a self-loaded track is Rust's.
//
// Its mode comes from the store too: the committing action resets it to shared. A native track is
// left to the delegate: it hears the SDK first and the store lags it by one confirmation, so
// following the store there too would send a stale mode over the one just applied. Changes are
// tracked on every notification, gated or not, so a self-loaded track only sends what changed
// during it.
export function followCommittedOutput(
	store: { getState(): any; subscribe(cb: () => void): () => void },
	rustPlaysForShaka: () => boolean = isSelfLoad,
): () => void {
	let last = keyOf(committedOutput(store.getState()));
	return store.subscribe(() => {
		const out = committedOutput(store.getState());
		const key = keyOf(out);
		if (key === last) return;
		last = key;
		if (out && rustPlaysForShaka()) selectDevice(out.id, out.mode);
	});
}
