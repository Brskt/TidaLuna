// Tests for `plugins/ui/src/DeviceDialogToggles.ts`.
//
// Mocked: `@luna/lib` exists only in the esbuild bundle. Its store is shaped like
// TIDAL's `player` slice, and its actions record what the toggles dispatch to TIDAL. The rule
// mapping that slice to a Rust device is the real one.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { committedOutput } from "../plugins/lib/src/redux/committedOutput";
import type { SentIpc } from "./setup";

const W = window as any;

const devices = [
	{ id: "default", name: "System Default", nativeDeviceId: "default", webDeviceId: "default" },
	{ id: "guid-b", name: "Headphones (B)", nativeDeviceId: "Headphones (B)", webDeviceId: "web-b" },
	// Listed by Chromium only: `mergeDevices` found no Rust device of that name.
	{ id: "guid-c", name: "Monitor (C)", webDeviceId: "web-c" },
];

const initialPlayer = () => ({ activeDeviceId: "default", activeDeviceMode: "shared", availableDevices: devices });
let state = { player: initialPlayer() };
const listeners = new Set<() => void>();
const dispatched: { type: string; payload: unknown }[] = [];
const ipcSent: SentIpc[] = [];
let onDialog: ((section: Element) => void) | undefined;

mock.module("@luna/lib", () => ({
	ipcRenderer: { send: (channel: string, ...args: unknown[]) => ipcSent.push({ channel, args }) },
	observe: (_unloads: unknown, _selector: string, cb: (section: Element) => void) => {
		onDialog = cb;
	},
	redux: {
		store: {
			getState: () => state,
			subscribe(cb: () => void) {
				listeners.add(cb);
				return () => listeners.delete(cb);
			},
		},
		actions: new Proxy({}, { get: (_, type: string) => (payload: unknown) => dispatched.push({ type, payload }) }),
		committedOutput,
	},
}));

const { setupDeviceDialogToggles } = await import("../plugins/ui/src/DeviceDialogToggles");

// A reducer ran and produced this `player` slice.
function commit(player: Partial<typeof state.player>) {
	state = { player: { ...state.player, ...player } };
	for (const cb of [...listeners]) cb();
}

// TIDAL renders its device dialog: the exclusive card, whose markup the ASIO row is cloned from.
function openDialog(): Element {
	const section = document.createElement("section");
	section.setAttribute("data-test", "dialog-device-settings");
	section.innerHTML =
		'<div class="card"><div class="flex"><div><span>Exclusive mode</span><span>info</span></div>' +
		'<label class="_unchecked_a1bcc16"><input type="checkbox" data-test="dialog-switch-exclusive-mode"></label>' +
		"</div></div>";
	document.body.append(section);
	onDialog?.(section);
	return section;
}

function flipAsio(section: Element, on: boolean) {
	const input = section.querySelector<HTMLInputElement>("#luna-asio-row input")!;
	input.checked = on;
	input.dispatchEvent(new Event("change"));
}

const deviceSets = () => ipcSent.filter((s) => s.channel === "player.devices.set").map((s) => s.args);

beforeEach(() => {
	state = { player: initialPlayer() };
	listeners.clear();
	dispatched.length = 0;
	ipcSent.length = 0;
	onDialog = undefined;
	W.__TIDALUNAR_PLATFORM__ = "win32";
	W.__TIDALUNAR_ASIO__ = false;
});

afterEach(() => {
	document.body.innerHTML = "";
	delete W.__TIDALUNAR_EXCLUSIVE__;
	W.__TIDALUNAR_ASIO__ = false;
});

test("the boot leaves the device mode to TIDAL, which restores it with its device", () => {
	// The flag the boot used to inject from Luna's own copy of the mode. Dispatched onto whatever
	// device was active, it committed exclusive on the system default.
	W.__TIDALUNAR_EXCLUSIVE__ = true;
	setupDeviceDialogToggles(new Set());
	expect(dispatched).toEqual([]);
});

test("exclusive turning on clears ASIO, and that is all it sends", () => {
	W.__TIDALUNAR_ASIO__ = true;
	setupDeviceDialogToggles(new Set());
	commit({ activeDeviceId: "guid-b", activeDeviceMode: "exclusive" });
	expect(W.__TIDALUNAR_ASIO__).toBe(false);
	expect(ipcSent).toEqual([{ channel: "settings.asio", args: [false] }]);
});

test("turning ASIO off keeps Rust on the device the dialog is open on", () => {
	// The gear, and so the dialog, only exists for a chosen device other than the system default.
	state = { player: { ...initialPlayer(), activeDeviceId: "guid-b" } };
	setupDeviceDialogToggles(new Set());
	const dialog = openDialog();
	flipAsio(dialog, true);
	// ASIO turned exclusive off through TIDAL: the mode TIDAL holds is shared.
	expect(dispatched).toEqual([{ type: "player/SET_DEVICE_MODE", payload: "shared" }]);
	flipAsio(dialog, false);
	expect(deviceSets()).toEqual([
		["auto", "asio"],
		["Headphones (B)", "shared"],
	]);
	expect(W.__TIDALUNAR_ASIO__).toBe(false);
});

test("turning ASIO off on a dialog left open over the system default sends the default", () => {
	// A disconnect moves TIDAL to the system default under the open dialog (NPD01).
	W.__TIDALUNAR_ASIO__ = true;
	state = { player: { ...initialPlayer(), activeDeviceId: "guid-b" } };
	setupDeviceDialogToggles(new Set());
	const dialog = openDialog();
	commit({ activeDeviceId: "default" });
	flipAsio(dialog, false);
	expect(deviceSets()).toEqual([["auto", "shared"]]);
});

test("turning ASIO off on a device only Chromium lists sends the default Rust falls back to", () => {
	W.__TIDALUNAR_ASIO__ = true;
	state = { player: { ...initialPlayer(), activeDeviceId: "guid-c" } };
	setupDeviceDialogToggles(new Set());
	flipAsio(openDialog(), false);
	expect(deviceSets()).toEqual([["auto", "shared"]]);
});
