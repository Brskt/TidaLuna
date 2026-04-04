import React, { useCallback, useState } from "react";

import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";

import { ipcRenderer } from "@luna/lib";

import { LunaButton, LunaSettings, LunaSwitchSetting } from "../../components";
import type { UpdateInfo } from "../../types/updater";

export const LunaUpdateSettings = React.memo(() => {
	const [checking, setChecking] = useState(false);
	const [result, setResult] = useState<UpdateInfo | null | undefined>(undefined);
	const [autoCheck, setAutoCheck] = useState(() => (window as any).__TIDALUNAR_AUTO_CHECK__ !== false);

	const handleCheck = useCallback(async () => {
		setChecking(true);
		setResult(undefined);
		try {
			const info = await ipcRenderer.invoke("updater.check");
			setResult(info ?? null);
		} catch {
			setResult(null);
		} finally {
			setChecking(false);
		}
	}, []);

	const handleAutoCheckToggle = useCallback((_: any, checked: boolean) => {
		setAutoCheck(checked);
		ipcRenderer.send("updater.set_auto_check", checked);
	}, []);

	const handleUpdate = useCallback(() => {
		if (result) {
			ipcRenderer.send("updater.apply", result.version);
		}
	}, [result]);

	return (
		<LunaSettings title="Updates">
			<Stack direction="row" alignItems="center" spacing={2}>
				<LunaButton
					loading={checking}
					onClick={handleCheck}
					variant="contained"
					sx={{ height: 40, textTransform: "none" }}
				>
					Check for updates
				</LunaButton>
				{result === null && (
					<Typography variant="body2" sx={{ color: "rgba(255,255,255,0.6)" }}>
						You're up to date.
					</Typography>
				)}
				{result && (
					<>
						<Typography variant="body2" sx={{ color: "#5b8def" }}>
							v{result.version} available ({result.changed_files} file{result.changed_files > 1 ? "s" : ""})
						</Typography>
						<LunaButton
							onClick={handleUpdate}
							variant="contained"
							color="success"
							sx={{ height: 40, textTransform: "none" }}
						>
							Update &amp; Restart
						</LunaButton>
					</>
				)}
			</Stack>
			<LunaSwitchSetting
				title="Auto-check on startup"
				desc="Check for updates automatically when the app starts"
				checked={autoCheck}
				onChange={handleAutoCheckToggle}
			/>
		</LunaSettings>
	);
});
