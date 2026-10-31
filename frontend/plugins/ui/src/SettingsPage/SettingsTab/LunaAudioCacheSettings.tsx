import React, { useCallback, useState } from "react";

import Slider from "@mui/material/Slider";

import { LunaButtonSetting, LunaSetting, LunaSettings, LunaSwitchSetting } from "../../components";
import {
	MAX_GB,
	MIN_GB,
	cacheAvailable,
	cacheNote,
	sizePosition,
	switchPosition,
	usageLine,
} from "../../audioCache/state";
import { clearAudioCache, setAudioCacheEnabled, setAudioCacheSize, useAudioCacheState } from "../../audioCache/store";

/** A second, quieter line under a setting's description. A span: `desc` renders inside a heading, where a div is not allowed. */
const Detail = ({ children }: { children: React.ReactNode }) => (
	<span style={{ display: "block", opacity: 0.7, marginTop: 4 }}>{children}</span>
);

export const LunaAudioCacheSettings = React.memo(() => {
	const state = useAudioCacheState();
	const usage = usageLine(state);
	const note = cacheNote(state);
	const available = cacheAvailable(state);
	// The value under the thumb while it moves; only the release is sent, a lowered cap
	// deleting files at every step otherwise.
	const [dragging, setDragging] = useState<number | null>(null);

	const handleToggle = useCallback((_: any, checked: boolean) => setAudioCacheEnabled(checked), []);
	const handleDrag = useCallback((_: Event, value: number) => setDragging(value), []);
	const handleRelease = useCallback((_: React.SyntheticEvent | Event, value: number) => {
		setDragging(null);
		setAudioCacheSize(value);
	}, []);

	return (
		<LunaSettings title="Audio cache" desc="Cache for streamed audio.">
			<LunaSwitchSetting
				title="Cache tracks"
				desc="Store streamed tracks in the cache."
				checked={switchPosition(state)}
				disabled={!available}
				onChange={handleToggle}
			/>
			<LunaSetting title="Cache size" desc="Space allocated for the cache.">
				<Slider
					aria-label="Audio cache size in gigabytes"
					min={MIN_GB}
					max={MAX_GB}
					step={1}
					marks
					disabled={!available}
					value={dragging ?? sizePosition(state)}
					onChange={handleDrag}
					onChangeCommitted={handleRelease}
					valueLabelDisplay="auto"
					valueLabelFormat={(v) => `${v} GB`}
					sx={{ marginLeft: "auto", maxWidth: 320 }}
				/>
			</LunaSetting>
			<LunaButtonSetting
				title="Clear cache"
				desc={
					<>
						Delete all cached tracks.
						{usage && <Detail>{usage}</Detail>}
						{note && <Detail>{note}</Detail>}
					</>
				}
				disabled={!available}
				onClick={clearAudioCache}
			>
				Clear
			</LunaButtonSetting>
		</LunaSettings>
	);
});
