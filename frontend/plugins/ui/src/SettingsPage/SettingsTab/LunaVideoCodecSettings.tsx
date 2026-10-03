import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";

import { LunaSettings, LunaSwitchSetting } from "../../components";

// Required verbatim by Cisco's binary licence, condition 3: it must appear where the user
// controls the codec. Not a credit line: the patent coverage is conditioned on it.
const ATTRIBUTION = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

export const LunaVideoCodecSettings = React.memo(() => {
	// `=== true`, not `!== false`: the Rust default is off, and an absent global must read as off too.
	const [enabled, setEnabled] = useState(() => (window as any).__TIDALUNAR_VIDEO_CODEC__ === true);

	const handleToggle = useCallback((_: any, checked: boolean) => {
		setEnabled(checked);
		// Mirrored before sending: this channel has no backend echo, and a tab remount would re-read
		// the stale boot value and spring the switch back.
		(window as any).__TIDALUNAR_VIDEO_CODEC__ = checked;
		ipcRenderer.send("settings.set_video_codec", checked);
	}, []);

	return (
		<LunaSettings title="Video" desc={ATTRIBUTION}>
			<LunaSwitchSetting
				title="Play animated covers"
				desc="Download Cisco's OpenH264 library and use it to play animated album covers. Applies on restart."
				checked={enabled}
				onChange={handleToggle}
			/>
		</LunaSettings>
	);
});
