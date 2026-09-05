import { useEffect, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import type { LunaUnloads } from "@luna/core";
import type { UpdateInfo } from "../types/updater";
import {
	hydrateUpdater,
	initialUpdaterState,
	reduceUpdater,
	type UpdaterEvent,
	type UpdaterState,
} from "./state";

export type { UpdaterEvent, UpdaterState } from "./state";

let current = initialUpdaterState;
const listeners = new Set<(state: UpdaterState) => void>();
let wired = false;

function publish(next: UpdaterState) {
	if (next === current) return;
	current = next;
	for (const listener of listeners) listener(current);
}

/**
 * Record a transition the renderer makes on its own.
 *
 * Only the phases the backend never announces belong here: it emits nothing when a download
 * starts. Everything it does announce is left to it: a refusal it makes is not painted
 * over by an optimistic click.
 */
export function pushUpdaterEvent(event: UpdaterEvent) {
	publish(reduceUpdater(current, event));
}

/**
 * Attach to the backend's updater events once, however many surfaces are mounted.
 *
 * Wired from the plugin's entry rather than lazily from a subscriber, for the listeners to
 * belong to the plugin's own unload set: a surface that mounts and unmounts must not take
 * the others' events with it.
 */
export function wireUpdater(unloads: LunaUnloads) {
	if (wired) return;
	wired = true;
	ipcRenderer.on(unloads, "updater.available", (info: UpdateInfo) =>
		pushUpdaterEvent({ kind: "available", info }),
	);
	ipcRenderer.on(unloads, "updater.ready", (version: string) =>
		pushUpdaterEvent({ kind: "ready", version }),
	);
	ipcRenderer.on(unloads, "updater.applying", () => pushUpdaterEvent({ kind: "applying" }));
	ipcRenderer.on(unloads, "updater.error", (message: string) =>
		pushUpdaterEvent({ kind: "error", message }),
	);
	ipcRenderer.on(unloads, "updater.cancelled", () => pushUpdaterEvent({ kind: "cancelled" }));
	// Announced rather than assumed by whoever clicked Skip: `updater.dismiss` is open to any
	// trusted frame, and a surface that kept the offer would hold a Download the backend now
	// refuses.
	ipcRenderer.on(unloads, "updater.dismissed", (version: string) =>
		pushUpdaterEvent({ kind: "dismissed", version }),
	);
	ipcRenderer.on(unloads, "updater.withheld", (reason: string) =>
		pushUpdaterEvent({ kind: "withheld", reason }),
	);
	ipcRenderer.on(unloads, "updater.channel_changed", () =>
		pushUpdaterEvent({ kind: "channel_changed" }),
	);
	unloads.add(() => {
		wired = false;
	});
	ipcRenderer
		.invoke("updater.status")
		.then((status: unknown) => publish(hydrateUpdater(current, status)))
		.catch(() => {});
}

/** Subscribe to the one record both surfaces read. */
export function onUpdaterState(listener: (state: UpdaterState) => void): () => void {
	listeners.add(listener);
	listener(current);
	return () => {
		listeners.delete(listener);
	};
}

/** The backend-owned half of a surface's state. Whatever else it tracks stays its own. */
export function useUpdaterState(): UpdaterState {
	const [state, setState] = useState<UpdaterState>(current);
	useEffect(() => onUpdaterState(setState), []);
	return state;
}
