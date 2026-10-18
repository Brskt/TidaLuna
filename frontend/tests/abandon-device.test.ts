// Tests for the delegate's `selectSystemDevice` against the TIDAL SDK calls that reach it.
//
// The fixture transcribes those paths from @tidal-music/player; the shipped bundle carries the
// same bodies.

import { afterEach, beforeEach, expect, test } from "bun:test";
import type { SentIpc } from "./setup";

const { SDK_OUTPUT_DEVICES } = await import("../src/output-device");
const { createNativePlayerComponent } = await import("../src/controllers/player");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const deviceSets = () => sent.filter((s) => s.channel === "player.devices.set").map((s) => s.args);
const W = window as any;

const DEFAULT = { id: "default", nativeDeviceId: "default", type: "systemDefault" };
const B = { id: "guid-b", nativeDeviceId: "Headphones (B)", type: "wasapi" };
const native = [{ id: "Headphones (B)", name: "Headphones (B)", controllableVolume: true }];

function tidalSdk() {
	const delegate = createNativePlayerComponent().Player();
	let activePlayer: any;
	let currentOutputId: string | undefined;
	let activeDevice: any = DEFAULT;
	let deviceMode = "shared";

	const nativePlayer = {
		name: "nativePlayer",
		abandon() {
			if (outputDevices.deviceMode === "exclusive") delegate.selectSystemDevice();
		},
		updateOutputDevice() {
			const sinkId = outputDevices.activeDevice.nativeDeviceId;
			if (sinkId === "default") {
				if (currentOutputId !== "default") {
					delegate.selectSystemDevice();
					currentOutputId = "default";
				}
			} else {
				const description = native.find((d) => d.id === sinkId);
				if (description) {
					delegate.selectDevice(description, outputDevices.deviceMode);
					currentOutputId = sinkId;
				}
			}
		},
		play() {
			this.updateOutputDevice();
		},
	};
	const shaka = { name: "shakaPlayer", updateOutputDevice() {} };

	const outputDevices = {
		set activeDevice(device: any) {
			activeDevice = device;
			deviceMode = "shared";
			activePlayer?.updateOutputDevice();
		},
		get activeDevice() {
			return activeDevice;
		},
		set deviceMode(mode: string) {
			if (activeDevice && activePlayer?.name === "nativePlayer" && deviceMode !== mode) {
				deviceMode = mode;
				activePlayer.updateOutputDevice();
			}
		},
		get deviceMode() {
			return deviceMode;
		},
	};
	W.__lunaHostModules = { [SDK_OUTPUT_DEVICES]: { outputDevices } };

	const setActivePlayer = (player: any) => {
		if (player.name !== "nativePlayer") nativePlayer.abandon();
		activePlayer = player;
	};
	return { nativePlayer, shaka, outputDevices, setActivePlayer };
}

// Device B picked, then TIDAL's exclusive switch, both during a native track.
function exclusiveOnB() {
	const sdk = tidalSdk();
	sdk.setActivePlayer(sdk.nativePlayer);
	sdk.outputDevices.activeDevice = B;
	sdk.outputDevices.deviceMode = "exclusive";
	sent.length = 0;
	return sdk;
}

beforeEach(() => {
	sent.length = 0;
	W.__TIDALUNAR_ASIO__ = false;
});

afterEach(() => {
	delete W.__lunaHostModules;
	W.__TIDALUNAR_ASIO__ = false;
});

test("the next native track takes exclusive back on the chosen device", () => {
	const sdk = exclusiveOnB();
	sdk.setActivePlayer(sdk.shaka);
	sdk.setActivePlayer(sdk.nativePlayer);
	sdk.nativePlayer.play();
	expect(deviceSets()).toEqual([
		["Headphones (B)", "shared"],
		["Headphones (B)", "exclusive"],
	]);
});

test("the system default reaches Rust as the default, shared, whichever way the SDK gets there", () => {
	const ways: [string, () => void][] = [
		// The setter resets the SDK's mode to shared before the delegate hears it.
		["picked from exclusive on B", () => void (exclusiveOnB().outputDevices.activeDevice = DEFAULT)],
		[
			"picked in shared mode",
			() => {
				const sdk = tidalSdk();
				sdk.setActivePlayer(sdk.nativePlayer);
				sdk.outputDevices.activeDevice = B;
				sent.length = 0;
				sdk.outputDevices.activeDevice = DEFAULT;
			},
		],
		[
			"a boot play on it",
			() => {
				const sdk = tidalSdk();
				sdk.setActivePlayer(sdk.nativePlayer);
				sdk.nativePlayer.play();
			},
		],
		[
			"an exclusive mode the SDK accepts on it",
			() => {
				const sdk = tidalSdk();
				sdk.setActivePlayer(sdk.nativePlayer);
				sdk.outputDevices.deviceMode = "exclusive";
			},
		],
		[
			// The SDK says exclusive on the default while Rust holds it shared; with the next native play
			// deduped by the SDK, the release is the only call Rust hears.
			"a release while it is active",
			() => {
				const sdk = tidalSdk();
				sdk.setActivePlayer(sdk.nativePlayer);
				sdk.nativePlayer.play();
				sdk.outputDevices.deviceMode = "exclusive";
				sent.length = 0;
				sdk.setActivePlayer(sdk.shaka);
				sdk.setActivePlayer(sdk.nativePlayer);
				sdk.nativePlayer.play();
			},
		],
	];
	for (const [way, run] of ways) {
		sent.length = 0;
		run();
		expect(deviceSets(), way).toEqual([["auto"]]);
	}
});

test("ASIO stays sticky through a release", () => {
	// ASIO toggled on while shaka played: TIDAL refused the mode change and still holds exclusive
	// under the ASIO flag.
	const sdk = exclusiveOnB();
	W.__TIDALUNAR_ASIO__ = true;
	sdk.setActivePlayer(sdk.shaka);
	expect(deviceSets()).toEqual([["auto", "asio"]]);
});

test("without the capture a release goes to the default, shared", () => {
	const sdk = exclusiveOnB();
	delete W.__lunaHostModules;
	sdk.setActivePlayer(sdk.shaka);
	expect(deviceSets()).toEqual([["auto"]]);
});
