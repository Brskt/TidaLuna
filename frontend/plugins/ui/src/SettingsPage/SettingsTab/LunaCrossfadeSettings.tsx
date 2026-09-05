import React, { useCallback, useState } from "react";

import Slider from "@mui/material/Slider";

import { ipcRenderer } from "@luna/lib";

import { LunaSetting, LunaSettings, LunaSwitchSetting } from "../../components";

const DEFAULT_SECS = 6;
const MAX_SECS = 12;

export const LunaCrossfadeSettings = React.memo(() => {
	const [enabled, setEnabled] = useState(() => (window as any).__TIDALUNAR_CROSSFADE_ENABLED__ === true);
	const [secs, setSecs] = useState(() => Number((window as any).__TIDALUNAR_CROSSFADE_SECS__ ?? 0));

	// Both values travel together on one channel: what the player needs is the
	// effective overlap, which is zero whenever the switch is off. Sending them
	// separately would leave the backend unable to compute it without reading back
	// whichever half did not just change.
	const push = useCallback((nextEnabled: boolean, nextSecs: number) => {
		setEnabled(nextEnabled);
		setSecs(nextSecs);
		(window as any).__TIDALUNAR_CROSSFADE_ENABLED__ = nextEnabled;
		(window as any).__TIDALUNAR_CROSSFADE_SECS__ = nextSecs;
		ipcRenderer.send("settings.set_crossfade", nextEnabled, nextSecs);
	}, []);

	const handleToggle = useCallback(
		(_: any, checked: boolean) => {
			// Turning it on with no duration yet would be a silent no-op. Seed the
			// value TIDAL's own switch seeds.
			push(checked, checked && secs === 0 ? DEFAULT_SECS : secs);
		},
		[secs, push],
	);

	// `step={1}` between 0 and MAX_SECS is what constrains this to whole seconds:
	// a slider cannot express a fraction: nothing downstream has to reject one.
	const handleSecs = useCallback((_: Event, value: number) => push(enabled, value), [enabled, push]);

	return (
		<LunaSettings title="Crossfade" desc="Adjust the length of fading and overlap in between tracks.">
			<LunaSwitchSetting
				title="Crossfade"
				desc="Blend the end of a track into the start of the next one."
				checked={enabled}
				onChange={handleToggle}
			/>
			{enabled && (
				<LunaSetting title="Overlap" desc="Seconds of overlap between tracks. Applies from the next track change.">
					<Slider
						aria-label="Crossfade overlap in seconds"
						min={0}
						max={MAX_SECS}
						step={1}
						value={secs}
						onChange={handleSecs}
						valueLabelDisplay="auto"
						valueLabelFormat={(v) => `${v}s`}
						sx={{ marginLeft: "auto", maxWidth: 320 }}
					/>
				</LunaSetting>
			)}
		</LunaSettings>
	);
});
