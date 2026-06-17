import Stack from "@mui/material/Stack";
import React from "react";

import { LunaSettings } from "../../components";

import { LunaPlugin } from "@luna/core";
import { LunaPluginSettings } from "../PluginsTab/LunaPluginSettings";
import { LunaAsioSettings } from "./LunaAsioSettings";
import { LunaConnectSettings } from "./LunaConnectSettings";
import { LunaFeatureFlags } from "./LunaFeatureFlags";
import { LunaLoggingSettings } from "./LunaLoggingSettings";
import { LunaSettingsTransfer } from "./LunaSettingsTransfer";
import { LunaUpdateSettings } from "./LunaUpdateSettings";
import { LunaVersionInfo } from "./LunaVersionInfo";
import { LunaVolumeSyncSettings } from "./LunaVolumeSyncSettings";

export const SettingsTab = React.memo(() => {
	const corePlugins = [];
	for (const plugin of Object.values(LunaPlugin.plugins)) {
		if (!LunaPlugin.corePlugins.has(plugin.name)) continue;
		corePlugins.push(<LunaPluginSettings key={plugin.url} plugin={plugin} />);
	}
	return (
		<Stack spacing={4}>
			<LunaVersionInfo />
			<LunaUpdateSettings />
			<LunaConnectSettings />
			<LunaVolumeSyncSettings />
			<LunaAsioSettings />
			<LunaSettingsTransfer />
			<LunaFeatureFlags />
			<LunaLoggingSettings />
			<LunaSettings title="Luna core plugins" desc="Plugins providing core luna functionality">
				{corePlugins}
			</LunaSettings>
		</Stack>
	);
});
