import { ipcRenderer, observe, redux } from "@luna/lib";

// Output toggles in TIDAL's native device-settings dialog ([data-test="dialog-device-settings"]).
//
// EXCLUSIVE is TIDAL's own switch, on a chosen device only. TIDAL saves the mode with
// that device (`selectedOutputDevice`) and restores both at boot. We only watch
// `player.activeDeviceMode` in TIDAL's Redux, for exclusive turning on to clear ASIO.
//
// ASIO has no TIDAL Redux concept, so we inject ONE row cloned from the native card
// (inherits TIDAL's exact styling) that we fully own. Mutual exclusion: enabling ASIO
// dispatches SET_DEVICE_MODE("shared") (Redux turns the native exclusive switch off);
// enabling exclusive clears ASIO via our store subscription. Windows-only.

const W = window as any;

// State classes for OUR cloned ASIO switch: `_checked_*` (on) / `_unchecked_*` (off) on
// the <label>. Learned from the native switches at runtime; literals are a fallback only.
let checkedCls: string | null = "_checked_3422d33";
let uncheckedCls: string | null = "_unchecked_a1bcc16";

function learnStateClasses(section: Element): void {
	for (const label of section.querySelectorAll("label")) {
		for (const c of label.classList) {
			if (c.startsWith("_checked")) checkedCls = c;
			else if (c.startsWith("_unchecked")) uncheckedCls = c;
		}
	}
}

function reflectSwitch(input: HTMLInputElement, checked: boolean): void {
	input.checked = checked;
	input.setAttribute("aria-checked", String(checked));
	const label = input.closest("label");
	if (!label) return;
	for (const c of Array.from(label.classList)) {
		if (c.startsWith("_checked") || c.startsWith("_unchecked")) label.classList.remove(c);
	}
	const target = checked ? checkedCls : uncheckedCls;
	if (target) label.classList.add(target);
}

export function setupDeviceDialogToggles(unloads: Set<() => void>): void {
	if (W.__TIDALUNAR_PLATFORM__ !== "win32") return;
	watchExclusiveMode(unloads);
	observe(unloads, '[data-test="dialog-device-settings"]', (section: Element) => {
		learnStateClasses(section);
		injectAsioRow(section);
	});
}

let lastMode: string | undefined;
function watchExclusiveMode(unloads: Set<() => void>): void {
	const sync = () => {
		let mode: string | undefined;
		try {
			mode = redux.store.getState()?.player?.activeDeviceMode;
		} catch {
			return; // store not ready yet
		}
		if (mode === undefined || mode === lastMode) return;
		lastMode = mode;
		const exclusive = mode === "exclusive";
		if (exclusive) {
			// Mutual exclusion: exclusive just turned on, clear ASIO.
			if (W.__TIDALUNAR_ASIO__ === true) {
				W.__TIDALUNAR_ASIO__ = false;
				ipcRenderer.send("settings.asio", false);
			}
			// Always reflect the ASIO clone off here: player.ts's selectDevice may have
			// already cleared __ASIO__ before this fires, so a flag-gated reflect would
			// miss it and leave the dialog switch visually stuck on.
			const asio = document.querySelector<HTMLInputElement>("#luna-asio-row input");
			if (asio) reflectSwitch(asio, false);
		}
	};
	try {
		unloads.add(redux.store.subscribe(sync));
	} catch {
		// store not ready; the delegate's exclusive selectDevice still clears the ASIO flag.
	}
	sync();
}

function injectAsioRow(section: Element): void {
	if (section.querySelector("#luna-asio-row")) return;
	const nativeEx = section.querySelector<HTMLInputElement>(
		'input[data-test="dialog-switch-exclusive-mode"]',
	);
	// The row "card" is input -> label -> flex -> card (see the native dialog markup).
	const nativeCard = nativeEx?.closest("label")?.parentElement?.parentElement as
		| HTMLElement
		| undefined;
	if (!nativeEx || !nativeCard) return;

	const row = nativeCard.cloneNode(true) as HTMLElement;
	row.id = "luna-asio-row";
	const spans = row.querySelectorAll("span");
	if (spans[0]) spans[0].textContent = "Use ASIO";
	if (spans[1]) {
		spans[1].textContent =
			"Output through an installed ASIO driver, bypassing the OS mixer.";
	}
	const input = row.querySelector("input") as HTMLInputElement;
	input.removeAttribute("data-test"); // not the native exclusive switch
	reflectSwitch(input, W.__TIDALUNAR_ASIO__ === true);
	input.addEventListener("change", () => {
		const on = input.checked;
		reflectSwitch(input, on);
		W.__TIDALUNAR_ASIO__ = on;
		ipcRenderer.send("settings.asio", on);
		if (on) {
			// Turn exclusive off via TIDAL's own Redux, which re-renders the native switch.
			try {
				redux.actions["player/SET_DEVICE_MODE"]("shared");
			} catch {}
			ipcRenderer.send("player.devices.set", "auto", "asio");
		} else {
			// Back to the device TIDAL holds, in its mode (the dialog opens on the committed device).
			// A Chromium-only device, which Rust cannot open, falls back to the system default here.
			const out = redux.committedOutput(redux.store.getState());
			ipcRenderer.send("player.devices.set", out?.id ?? "auto", out?.mode ?? "shared");
		}
	});

	// Insert just below the native exclusive row (above Force volume).
	nativeCard.after(row);
}
