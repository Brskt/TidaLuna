import React, { useCallback, useEffect, useState } from "react";

import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";

import { ipcRenderer } from "@luna/lib";

import { LunaButton, LunaSettings, LunaSwitchSetting } from "../../components";
import type { UpdateInfo, UpdaterPhase } from "../../types/updater";

function formatSize(bytes: number): string {
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

type SettingsPhase = "idle" | "checking" | UpdaterPhase;

export const LunaUpdateSettings = React.memo(() => {
	const [phase, setPhase] = useState<SettingsPhase>("idle");
	const [result, setResult] = useState<UpdateInfo | null>(null);
	const [errorMsg, setErrorMsg] = useState<string>("");
	const [autoCheck, setAutoCheck] = useState(() => (window as any).__TIDALUNAR_AUTO_CHECK__ !== false);

	useEffect(() => {
		ipcRenderer.invoke("updater.status").then((status: any) => {
			if (status?.state === "Ready" && status?.version) {
				setResult({ version: status.version, download_size: 0 });
				setPhase("ready");
			} else if (status?.state === "Downloading") {
				setPhase("downloading");
			} else if (status?.last_info) {
				setResult(status.last_info);
				setPhase("available");
			}
		}).catch(() => {});
	}, []);

	useEffect(() => {
		const unsubReady = __ipcRenderer.on("updater.ready", () => {
			setPhase("ready");
		});
		const unsubError = __ipcRenderer.on("updater.error", (msg: string) => {
			setErrorMsg(msg);
			setPhase("error");
		});
		const unsubCancel = __ipcRenderer.on("updater.cancelled", () => {
			setPhase((prev) => (prev === "downloading" ? "available" : prev));
		});
		return () => {
			unsubReady();
			unsubError();
			unsubCancel();
		};
	}, []);

	const handleCheck = useCallback(async () => {
		setPhase("checking");
		setResult(null);
		try {
			const info = await ipcRenderer.invoke("updater.check");
			if (info) {
				setResult(info);
				setPhase("available");
			} else {
				setResult(null);
				setPhase("idle");
			}
		} catch {
			setPhase("idle");
		}
	}, []);

	const handleDownload = useCallback(async () => {
		if (!result) return;
		setPhase("downloading");
		try {
			const res = await ipcRenderer.invoke("updater.download", result.version);
			if (res === "already_ready") {
				setPhase("ready");
			}
		} catch (err: any) {
			const msg: string = typeof err === "string" ? err : (err?.message ?? "");
			if (msg === "download_in_progress") {
				// Events from the existing download will transition us
			} else {
				setErrorMsg(msg || "Download failed");
				setPhase("error");
			}
		}
	}, [result]);

	const handleCancel = useCallback(() => {
		ipcRenderer.send("updater.cancel");
	}, []);

	const handleRestart = useCallback(() => {
		if (result) {
			ipcRenderer.send("updater.apply", result.version);
		}
	}, [result]);

	const handleAutoCheckToggle = useCallback((_: any, checked: boolean) => {
		setAutoCheck(checked);
		ipcRenderer.send("updater.set_auto_check", checked);
	}, []);

	return (
		<LunaSettings title="Updates">
			<Stack direction="row" alignItems="center" spacing={2}>
				<LunaButton
					loading={phase === "checking"}
					onClick={handleCheck}
					disabled={phase === "downloading" || phase === "ready"}
					variant="contained"
					sx={{ height: 40, textTransform: "none" }}
				>
					Check for updates
				</LunaButton>
				{phase === "idle" && (
					<Typography variant="body2" sx={{ color: "rgba(255,255,255,0.6)" }}>
						You're up to date.
					</Typography>
				)}
				{phase === "available" && result && (
					<>
						<Typography variant="body2" sx={{ color: "#5b8def" }}>
							v{result.version} available ({formatSize(result.download_size)})
						</Typography>
						<LunaButton
							onClick={handleDownload}
							variant="contained"
							color="success"
							sx={{ height: 40, textTransform: "none" }}
						>
							Download
						</LunaButton>
					</>
				)}
				{phase === "downloading" && (
					<>
						<Typography variant="body2" sx={{ color: "rgba(255,255,255,0.6)" }}>
							Downloading...
						</Typography>
						<LunaButton
							onClick={handleCancel}
							variant="contained"
							sx={{ height: 40, textTransform: "none" }}
						>
							Cancel
						</LunaButton>
					</>
				)}
				{phase === "ready" && (
					<>
						<Typography variant="body2" sx={{ color: "#1db954" }}>
							Ready to update
						</Typography>
						<LunaButton
							onClick={handleRestart}
							variant="contained"
							color="success"
							sx={{ height: 40, textTransform: "none" }}
						>
							Apply &amp; Restart
						</LunaButton>
					</>
				)}
				{phase === "error" && (
					<>
						<Typography variant="body2" sx={{ color: "#f44336" }}>
							{errorMsg || "Download failed"}
						</Typography>
						<LunaButton
							onClick={handleDownload}
							variant="contained"
							color="error"
							sx={{ height: 40, textTransform: "none" }}
						>
							Retry
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
