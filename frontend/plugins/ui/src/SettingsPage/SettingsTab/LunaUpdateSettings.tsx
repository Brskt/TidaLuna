import React, { useCallback, useState } from "react";

import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";

import { ipcRenderer } from "@luna/lib";

import { LunaButton, LunaSelectItem, LunaSelectSetting, LunaSettings, LunaSwitchSetting } from "../../components";
import {
	eventForDownloadFailure,
	eventForDownloadReply,
	type UpdaterPhase,
} from "../../updater/state";
import { pushUpdaterEvent, useUpdaterState } from "../../updater/store";

function formatSize(bytes: number): string {
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** The shared phases plus the one this surface owns alone: the check it is running. */
type SettingsPhase = "checking" | UpdaterPhase;

export const LunaUpdateSettings = React.memo(() => {
	const shared = useUpdaterState();
	// Only what this surface owns alone stays local: the phase the backend never publishes,
	// and what the manual check had to report to whoever clicked it. Neither means anything
	// to the toast. Everything else (which update, how far along, what failed) comes from
	// the record every surface reads; a transition made here cannot leave the other
	// showing the state before it.
	const [checking, setChecking] = useState(false);
	const [checkNote, setCheckNote] = useState("");
	const [autoCheck, setAutoCheck] = useState(() => (window as any).__TIDALUNAR_AUTO_CHECK__ !== false);
	const [channel, setChannel] = useState(() =>
		(window as any).__TIDALUNAR_UPDATE_CHANNEL__ === "dev" ? "dev" : "stable",
	);
	const result = shared.info;
	const errorMsg = shared.errorMsg;
	// The record's phase stands on its own. Deriving it from whether an offer was present is
	// what turned an in-flight download into "You're up to date." here: a reload hydrates the
	// phase from the backend, and this surface threw it away for want of an `info` beside it.
	const phase: SettingsPhase = checking ? "checking" : shared.phase;

	const handleCheck = useCallback(async () => {
		setChecking(true);
		setCheckNote("");
		try {
			const outcome = await ipcRenderer.invoke("updater.check");
			if (outcome?.outcome === "available") {
				pushUpdaterEvent({ kind: "available", info: outcome.info });
			} else {
				// Up to date and withheld both say nothing is installable here, and the
				// record is what the toast reads as well. Dropping this answer is what kept
				// a ruled-out version on both surfaces, with no way back to "up to date".
				pushUpdaterEvent({ kind: "not_available" });
				if (outcome?.outcome === "withheld") setCheckNote(outcome.reason ?? "");
			}
		} catch (err: any) {
			// A check answered for a channel that has since changed is refused, and the
			// refusal is not this surface's news to report: `updater.channel_changed` has
			// already reset every record, and the note handleChannel left standing says what
			// to do next. Keyed on the code, never the message: matching a protocol string
			// by hand is how "applying" once reached a user as the error text "applying".
			if (err?.code === 409) return;
			// A check that never concluded disproves nothing: the shared record stands,
			// and only this surface, the one that asked, carries the news.
			const msg: string = typeof err === "string" ? err : (err?.message ?? "");
			setCheckNote(msg || "Check failed");
		} finally {
			setChecking(false);
		}
	}, []);

	const handleDownload = useCallback(async () => {
		if (!result) return;
		pushUpdaterEvent({ kind: "downloading" });
		try {
			const reply = await ipcRenderer.invoke("updater.download", result.version);
			const event = eventForDownloadReply(reply, result.version);
			if (event) pushUpdaterEvent(event);
		} catch (err) {
			const event = eventForDownloadFailure(err);
			if (event) pushUpdaterEvent(event);
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
		(window as any).__TIDALUNAR_AUTO_CHECK__ = checked;
		ipcRenderer.send("updater.set_auto_check", checked);
	}, []);

	const handleChannel = useCallback((value: string) => {
		const channel = value === "dev" ? "dev" : "stable";
		setChannel(channel);
		(window as any).__TIDALUNAR_UPDATE_CHANNEL__ = channel;
		ipcRenderer.send("updater.set_channel", channel);
		// The record is cleared by the backend's own `updater.channel_changed`, which reaches
		// every surface rather than only this one. What stays local is the line telling
		// whoever flipped the switch why the panel went quiet.
		setCheckNote("Channel changed. Check again to see what it offers.");
	}, []);

	return (
		<LunaSettings title="Updates">
			<Stack direction="row" alignItems="center" spacing={2}>
				<LunaButton
					loading={phase === "checking"}
					onClick={handleCheck}
					disabled={phase === "downloading" || phase === "ready" || phase === "applying"}
					variant="contained"
					sx={{ height: 40, textTransform: "none" }}
				>
					Check for updates
				</LunaButton>
				{/* What this surface was told, in order of who it came from: the answer to the
				    check the user just ran, then the reason the backend volunteered for
				    holding a version back, then the plain state. */}
				{(checkNote || shared.withheldReason || phase === "idle") && (
					<Typography variant="body2" sx={{ color: "rgba(255,255,255,0.6)" }}>
						{checkNote || shared.withheldReason || "You're up to date."}
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
				{(phase === "ready" || phase === "applying") && (
					<>
						<Typography variant="body2" sx={{ color: "#1db954" }}>
							{phase === "applying" ? "Applying update..." : "Ready to update"}
						</Typography>
						<LunaButton
							onClick={handleRestart}
							loading={phase === "applying"}
							disabled={phase === "applying"}
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
							{errorMsg}
						</Typography>
						{/* A failure is reported whether or not an offer was ever known, and there is
						    nothing to retry when none was: `handleDownload` reads the offer and
						    returns on its absence, so the button offered a second chance it could
						    not take. The message stands on its own. */}
						{result && (
							<LunaButton
								onClick={handleDownload}
								variant="contained"
								color="error"
								sx={{ height: 40, textTransform: "none" }}
							>
								Retry
							</LunaButton>
						)}
					</>
				)}
			</Stack>
			<LunaSwitchSetting
				title="Auto-check on startup"
				desc="Check for updates automatically when the app starts"
				checked={autoCheck}
				onChange={handleAutoCheckToggle}
			/>
			<LunaSelectSetting
				title="Update channel"
				desc="Stable ships released versions only. Dev also includes pre-release builds that may be unstable or buggy."
				value={channel}
				onChange={(e) => handleChannel(String(e.target.value))}
				sx={{ width: 160, flexGrow: 0 }}
			>
				<LunaSelectItem value="stable">Stable</LunaSelectItem>
				<LunaSelectItem value="dev">Dev</LunaSelectItem>
			</LunaSelectSetting>
		</LunaSettings>
	);
});
