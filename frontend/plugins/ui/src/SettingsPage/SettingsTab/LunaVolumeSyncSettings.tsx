import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";

import { LunaSettings, LunaSwitchSetting } from "../../components";

export const LunaVolumeSyncSettings = React.memo(() => {
	if ((window as any).__TIDALUNAR_PLATFORM__ !== "win32") return null;

	const [syncEnabled, setSyncEnabled] = useState(() => (window as any).__TIDALUNAR_VOLUME_SYNC__ !== false);

	const handleToggle = useCallback((_: any, checked: boolean) => {
		setSyncEnabled(checked);
		(window as any).__TIDALUNAR_VOLUME_SYNC__ = checked;
		ipcRenderer.send("settings.volume_sync", checked);
	}, []);

	return (
		<LunaSettings title="System volume" desc="Mirror playback volume to the Windows system mixer.">
			<LunaSwitchSetting
				title="Sync Volume with OS"
				desc="Keep TidaLunar's volume in sync with the Windows volume slider."
				checked={syncEnabled}
				onChange={handleToggle}
			/>
		</LunaSettings>
	);
});
