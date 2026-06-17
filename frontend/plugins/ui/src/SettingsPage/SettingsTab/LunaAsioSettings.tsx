import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";

import { LunaSettings, LunaSwitchSetting } from "../../components";

export const LunaAsioSettings = React.memo(() => {
	if ((window as any).__TIDALUNAR_PLATFORM__ !== "win32") return null;

	const [asioEnabled, setAsioEnabled] = useState(() => (window as any).__TIDALUNAR_ASIO__ === true);

	const handleToggle = useCallback((_: any, checked: boolean) => {
		setAsioEnabled(checked);
		(window as any).__TIDALUNAR_ASIO__ = checked;
		// Persist the choice so it survives a restart (mirrors Volume Sync). On next boot
		// Rust seeds __TIDALUNAR_ASIO__ and the sticky device logic re-asserts ASIO.
		ipcRenderer.send("settings.asio", checked);
		// "auto" id (the ASIO backend picks its own driver); the mode arg drives the Rust
		// output path, and the radio invariant is enforced in Rust. Disabling returns to shared.
		if (checked) {
			ipcRenderer.send("player.devices.set", "auto", "asio");
		} else {
			ipcRenderer.send("player.devices.set", "auto");
		}
	}, []);

	return (
		<LunaSettings
			title="ASIO output (experimental)"
			desc="Output through an installed ASIO driver, bypassing the OS mixer. Windows only."
		>
			<LunaSwitchSetting
				title="Use ASIO output"
				desc="Takes priority over exclusive WASAPI while on. Turn off to return to shared output."
				checked={asioEnabled}
				onChange={handleToggle}
			/>
		</LunaSettings>
	);
});
