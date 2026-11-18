import React from "react";

import { LunaPlugin, Tracer } from "@luna/core";

import Button from "@mui/material/Button";
import Tooltip from "@mui/material/Tooltip";
import CachedIcon from "@mui/icons-material/Cached";

const { trace } = Tracer("[@luna/ui]", null);

type ReloadAllResult = {
	reloaded: string[];
	notReloaded: { name: string; reason: string }[];
};

// Set on the class by the host's loader: the upstream LunaPlugin type does not declare them.
const host = LunaPlugin as unknown as {
	reloadAll?: () => Promise<ReloadAllResult>;
	reloadAllInFlight?: () => Promise<ReloadAllResult> | undefined;
};

const report = ({ notReloaded }: ReloadAllResult) => {
	if (notReloaded.length === 0) trace.msg.log("Reloaded all plugins");
	else trace.msg.warn(`Reloaded plugins, ${notReloaded.length} not reloaded: ${notReloaded.map((p) => `${p.name} (${p.reason})`).join(", ")}`);
};

export const ReloadAllPlugins = React.memo(() => {
	const [reloading, setReloading] = React.useState(false);

	// A run outlives this tab: one already going shows here and settles the button.
	const follow = React.useCallback(async (run: Promise<ReloadAllResult>) => {
		setReloading(true);
		try {
			report(await run);
		} catch (err) {
			trace.msg.err.withContext("Reload all failed")(err);
		} finally {
			setReloading(false);
		}
	}, []);

	React.useEffect(() => {
		const run = host.reloadAllInFlight?.();
		if (run) void follow(run);
	}, [follow]);

	const onClick = React.useCallback(() => {
		if (host.reloadAll) void follow(host.reloadAll());
	}, [follow]);

	return (
		<Tooltip title="Update and restart every enabled plugin">
			<span style={{ alignSelf: "flex-end" }}>
				<Button
					size="small"
					color="warning"
					variant="outlined"
					disabled={reloading}
					onClick={onClick}
					startIcon={
						<CachedIcon
							sx={{
								animation: reloading ? "luna-spin 0.5s linear infinite" : "none",
								"@keyframes luna-spin": { "0%": { transform: "rotate(0deg)" }, "100%": { transform: "rotate(360deg)" } },
							}}
						/>
					}
				>
					Reload all
				</Button>
			</span>
		</Tooltip>
	);
});
