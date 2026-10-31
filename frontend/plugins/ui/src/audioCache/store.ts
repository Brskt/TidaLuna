import { useEffect, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import { createStore } from "../store";
import { wireSnapshot } from "../wireSnapshot";
import {
	acceptSnapshot,
	clickSwitch,
	initialAudioCacheState,
	pickSize,
	settleClick,
	type AudioCacheState,
	type ClickOutcome,
	type Control,
} from "./state";

/** The backend's event and the request the page pulls with: one name both ways. */
const CHANNEL = "settings.audio_cache_state";

const store = createStore(initialAudioCacheState);
const { publish } = store;

/**
 * Attach to the backend's cache snapshots once, from the plugin's entry: stores land while the
 * settings tab is closed, and it should open on the current usage. The pull makes up for any push
 * sent before this listened.
 */
export const wireAudioCache = wireSnapshot(store, CHANNEL, acceptSnapshot);

/** Whether the backend answers a click with the state it left or refuses it, the click is over. */
function settle(control: Control, click: number, outcome: ClickOutcome) {
	publish(settleClick(store.get(), control, click, outcome));
}

/** Held here rather than in the tab, the clicks survive a remount before the answer. */
export function setAudioCacheEnabled(on: boolean) {
	publish(clickSwitch(store.get(), on));
	const click = store.get().enabledClick;
	ipcRenderer.invoke("settings.set_audio_cache_enabled", on).then(
		(reply: unknown) => settle("enabled", click, { reply }),
		() => settle("enabled", click, "rejected"),
	);
}

export function setAudioCacheSize(gb: number) {
	publish(pickSize(store.get(), gb));
	const click = store.get().gbClick;
	ipcRenderer.invoke("settings.set_audio_cache_size", gb).then(
		(reply: unknown) => settle("gb", click, { reply }),
		() => settle("gb", click, "rejected"),
	);
}

export function clearAudioCache() {
	ipcRenderer.send("settings.clear_audio_cache");
}

export function useAudioCacheState(): AudioCacheState {
	const [state, setState] = useState<AudioCacheState>(store.get());
	useEffect(() => {
		const unsubscribe = store.subscribe(setState);
		setState(store.get());
		return unsubscribe;
	}, []);
	return state;
}
