import React from "react";

import Stack from "@mui/material/Stack";

import { LunaPlugin } from "@luna/core";
import { LunaPluginSettings } from "./LunaPluginSettings";

export const PluginsTab = React.memo(() => {
	const [, forceUpdate] = React.useReducer((x: number) => x + 1, 0);
	React.useEffect(() => {
		const handler = () => forceUpdate();
		window.addEventListener("luna:plugins-updated", handler);
		return () => window.removeEventListener("luna:plugins-updated", handler);
	}, []);

	const plugins = [];
	for (const plugin of Object.values(LunaPlugin.plugins)) {
		if (LunaPlugin.corePlugins.has(plugin.name)) continue;
		// Only show installed plugins (not store display instances)
		if (!plugin.installed) continue;
		plugins.push(<LunaPluginSettings key={plugin.url} plugin={plugin} />);
	}
	if (plugins.length === 0) return "You have no plugins installed!";
	return <Stack spacing={2}>{plugins}</Stack>;
});
