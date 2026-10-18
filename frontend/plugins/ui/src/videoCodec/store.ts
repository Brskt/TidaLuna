import { useEffect, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import type { LunaUnloads } from "@luna/core";
import { acceptSnapshot, clickSwitch, initialVideoCodecState, type VideoCodecState } from "./state";

/** The backend's event and the request the page pulls with: one name both ways. */
const CHANNEL = "settings.video_codec_state";

let current = initialVideoCodecState;
const listeners = new Set<(state: VideoCodecState) => void>();
let wired = false;

function publish(next: VideoCodecState) {
	if (next === current) return;
	current = next;
	for (const listener of listeners) listener(current);
}

/**
 * Attach to the backend's codec snapshots once, from the plugin's entry, early enough for the state
 * to be there when the settings tab mounts. The pull makes up for any push sent before this listened.
 */
export function wireVideoCodec(unloads: LunaUnloads) {
	if (wired) return;
	wired = true;
	ipcRenderer.on(unloads, CHANNEL, (snapshot: unknown) => publish(acceptSnapshot(current, snapshot)));
	unloads.add(() => {
		wired = false;
	});
	ipcRenderer
		.invoke(CHANNEL)
		.then((snapshot: unknown) => publish(acceptSnapshot(current, snapshot)))
		.catch(() => {});
}

/** The click is held here rather than in the tab: a remount before the echo keeps it. */
export function setVideoCodecEnabled(on: boolean) {
	publish(clickSwitch(current, on));
	ipcRenderer.send("settings.set_video_codec", on);
}

export function useVideoCodecState(): VideoCodecState {
	const [state, setState] = useState<VideoCodecState>(current);
	useEffect(() => {
		listeners.add(setState);
		setState(current);
		return () => {
			listeners.delete(setState);
		};
	}, []);
	return state;
}
