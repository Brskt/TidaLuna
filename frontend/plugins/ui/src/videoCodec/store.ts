import { useEffect, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import { createStore } from "../store";
import { wireSnapshot } from "../wireSnapshot";
import { acceptSnapshot, clickSwitch, initialVideoCodecState, settleClick, type VideoCodecState } from "./state";

/** The backend's event and the request the page pulls with: one name both ways. */
const CHANNEL = "settings.video_codec_state";

const store = createStore(initialVideoCodecState);
const { publish } = store;

/**
 * Attach to the backend's codec snapshots once, from the plugin's entry, early enough for the state
 * to be there when the settings tab mounts. The pull makes up for any push sent before this listened.
 */
export const wireVideoCodec = wireSnapshot(store, CHANNEL, acceptSnapshot);

/**
 * The click is held here rather than in the tab: a remount before the answer keeps it. The answer
 * waits for the step that takes the click, a download under way included.
 */
export function setVideoCodecEnabled(on: boolean) {
	publish(clickSwitch(store.get(), on));
	const click = store.get().click;
	ipcRenderer.invoke("settings.set_video_codec", on).then(
		(reply: unknown) => publish(settleClick(store.get(), click, { reply })),
		() => publish(settleClick(store.get(), click, "rejected")),
	);
}

export function useVideoCodecState(): VideoCodecState {
	const [state, setState] = useState<VideoCodecState>(store.get());
	useEffect(() => {
		const unsubscribe = store.subscribe(setState);
		setState(store.get());
		return unsubscribe;
	}, []);
	return state;
}
