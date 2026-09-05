import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import { dismissKey, eventForDownloadFailure, eventForDownloadReply } from "../updater/state";
import { pushUpdaterEvent, useUpdaterState } from "../updater/store";

function formatSize(bytes: number): string {
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

const containerStyle: React.CSSProperties = {
	position: "fixed",
	top: 100,
	right: 16,
	zIndex: 99999,
	background: "rgba(30, 30, 30, 0.4)",
	backdropFilter: "blur(20px)",
	WebkitBackdropFilter: "blur(20px)",
	borderRadius: 10,
	border: "1px solid rgba(255, 255, 255, 0.12)",
	color: "#fff",
	fontFamily: "system-ui, -apple-system, sans-serif",
	minWidth: 300,
	maxWidth: 360,
	padding: "14px 16px",
	boxShadow: "0 8px 40px rgba(0, 0, 0, 0.6)",
};

const headerStyle: React.CSSProperties = {
	display: "flex",
	alignItems: "center",
	justifyContent: "space-between",
	marginBottom: 6,
};

const titleStyle: React.CSSProperties = {
	fontWeight: 600,
	fontSize: 13,
	color: "#fff",
	display: "flex",
	alignItems: "center",
	gap: 8,
};

const subtitleStyle: React.CSSProperties = {
	fontSize: 12,
	color: "#999",
	marginBottom: 12,
};

const btnRowStyle: React.CSSProperties = {
	display: "flex",
	gap: 8,
};

const btnBase: React.CSSProperties = {
	border: "none",
	borderRadius: 4,
	color: "#fff",
	cursor: "pointer",
	fontSize: 13,
	fontFamily: "inherit",
	padding: "7px 14px",
};

export const UpdateToast: React.FC = () => {
	const state = useUpdaterState();
	const { info, phase, errorMsg } = state;
	// Dismissal is this surface's own business, not the backend's: the settings page shows
	// the same update and must not lose it because the toast was closed. Keyed by `dismissKey`
	// rather than by the version alone: the next release raises the toast again while a
	// re-announced one stays down, and a failure that never had an offer still has something to
	// be put down by.
	const [dismissed, setDismissed] = useState<string | null>(null);

	const handleDownload = useCallback(async () => {
		if (!info) return;
		// The one phase the backend never announces. The renderer records it, and records
		// it where every surface reads it, not where only this one does.
		pushUpdaterEvent({ kind: "downloading" });
		try {
			const reply = await ipcRenderer.invoke("updater.download", info.version);
			const event = eventForDownloadReply(reply, info.version);
			if (event) pushUpdaterEvent(event);
		} catch (err) {
			const event = eventForDownloadFailure(err);
			if (event) pushUpdaterEvent(event);
		}
	}, [info]);

	const handleCancel = useCallback(() => {
		ipcRenderer.send("updater.cancel");
	}, []);

	// The toast stays up. Dismissing here read as though the restart had already happened,
	// but this is a request, and the app is still running until the updater child actually
	// starts. When it does not, `updater.error` arrives to a component that has rendered
	// nothing since the click, and the user is left with an update that silently never
	// applied. Dismissal belongs to Skip and Close, which the user chooses.
	//
	// The phase is left to the backend's `updater.applying`, which it emits whether it claims
	// this apply or refuses it as one already in flight. Painting it optimistically here is
	// what hid that refusal from the surface that asked for it.
	const handleRestart = useCallback(() => {
		if (info) ipcRenderer.send("updater.apply", info.version);
	}, [info]);

	const handleSkip = useCallback(() => {
		if (info) ipcRenderer.send("updater.dismiss", info.version);
		setDismissed(dismissKey(state));
	}, [info, state]);

	const handleClose = useCallback(() => {
		setDismissed(dismissKey(state));
	}, [state]);

	// Silent only when the record holds nothing at all. Keying this on the offer instead is
	// what took the toast down for the length of a download whose version the renderer had
	// not learned, and the only Cancel control went down with it.
	if (phase === "idle") return null;
	if (dismissKey(state) === dismissed) return null;

	return (
		<div style={containerStyle}>
			<div style={headerStyle}>
				<div style={titleStyle}>
					<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#31d8ff" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
						<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
						<polyline points="7 10 12 15 17 10" />
						<line x1="12" y1="15" x2="12" y2="3" />
					</svg>
					{phase === "ready" || phase === "applying"
						? "Ready to restart"
						: info
							? `TidaLunar v${info.version} available`
							: "Update failed"}
				</div>
				<button
					onClick={handleClose}
					style={{ background: "none", border: "none", color: "#666", cursor: "pointer", padding: 2, fontSize: 16, lineHeight: 1 }}
				>
					✕
				</button>
			</div>
			<div style={subtitleStyle}>
				{phase === "available" && info && `Download size: ${formatSize(info.download_size)}`}
				{phase === "downloading" && "Downloading update..."}
				{phase === "ready" && "Update downloaded and ready to install."}
				{phase === "applying" && "Applying update..."}
				{phase === "error" && errorMsg}
			</div>
			<div style={btnRowStyle}>
				{phase === "available" && (
					<>
						<button onClick={handleDownload} style={{ ...btnBase, background: "#eb1e32" }}>
							Update
						</button>
						<button onClick={handleSkip} style={{ ...btnBase, background: "#333" }}>
							Skip
						</button>
					</>
				)}
				{phase === "downloading" && (
					<button onClick={handleCancel} style={{ ...btnBase, background: "#333" }}>
						Cancel
					</button>
				)}
				{phase === "ready" && (
					<button onClick={handleRestart} style={{ ...btnBase, background: "#1db954" }}>
						Apply &amp; Restart
					</button>
				)}
				{phase === "applying" && (
					<button disabled style={{ ...btnBase, background: "#333", cursor: "default" }}>
						Restarting...
					</button>
				)}
				{phase === "error" && info && (
					<button onClick={handleDownload} style={{ ...btnBase, background: "#eb1e32" }}>
						Retry
					</button>
				)}
			</div>
		</div>
	);
};
