import React, { useCallback } from "react";

import { LunaSettings, LunaSwitchSetting } from "../../components";
import { codecRowNote, switchPosition } from "../../videoCodec/state";
import { setVideoCodecEnabled, useVideoCodecState } from "../../videoCodec/store";

// Required verbatim by Cisco's binary licence, condition 3: it must appear where the user
// controls the codec. Not a credit line: the patent coverage is conditioned on it.
const ATTRIBUTION = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

export const LunaVideoCodecSettings = React.memo(() => {
	const state = useVideoCodecState();
	const note = codecRowNote(state);

	const handleToggle = useCallback((_: any, checked: boolean) => setVideoCodecEnabled(checked), []);

	return (
		<LunaSettings title="Video" desc={ATTRIBUTION}>
			<LunaSwitchSetting
				title="Play animated covers"
				desc={
					<>
						Download Cisco's OpenH264 library and use it to play animated album covers.
						{/* A span: `desc` renders inside a heading, where a div is not allowed. */}
						{note && <span style={{ display: "block", opacity: 0.7, marginTop: 4 }}>{note}</span>}
					</>
				}
				checked={switchPosition(state)}
				onChange={handleToggle}
			/>
		</LunaSettings>
	);
});
