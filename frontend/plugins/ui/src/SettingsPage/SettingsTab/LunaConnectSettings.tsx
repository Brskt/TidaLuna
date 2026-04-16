import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";

import { LunaSettings, LunaSwitchSetting } from "../../components";

export const LunaConnectSettings = React.memo(() => {
	const [alwaysOn, setAlwaysOn] = useState(() => (window as any).__TIDALUNAR_RECEIVER_ALWAYS_ON__ !== false);

	const handleToggle = useCallback((_: any, checked: boolean) => {
		setAlwaysOn(checked);
		(window as any).__TIDALUNAR_RECEIVER_ALWAYS_ON__ = checked;
		ipcRenderer.send("connect.receiver.set_always_on", checked);
	}, []);

	return (
		<LunaSettings title="TIDAL Connect" desc="Network discoverability for TIDAL Connect receiver.">
			<LunaSwitchSetting
				title="Always discoverable"
				desc="Allow this device to be discovered as a TIDAL Connect target at all times."
				checked={alwaysOn}
				onChange={handleToggle}
			/>
		</LunaSettings>
	);
});
