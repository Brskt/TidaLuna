import React, { useCallback, useEffect, useRef, useState } from "react";

import { ipcRenderer } from "@luna/lib";
import type { LunaUnloads } from "@luna/core";
import type { UpdateInfo, UpdaterPhase } from "../types/updater";

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

export const UpdateToast: React.FC<{ unloads: LunaUnloads }> = ({ unloads }) => {
	const [info, setInfo] = useState<UpdateInfo | null>(null);
	const [phase, setPhase] = useState<UpdaterPhase>("available");
	const [errorMsg, setErrorMsg] = useState<string>("");
	const infoRef = useRef<UpdateInfo | null>(null);

	useEffect(() => {
		infoRef.current = info;
	}, [info]);

	useEffect(() => {
		ipcRenderer.invoke("updater.status").then((status: any) => {
			if (status?.state === "Ready" && status?.version) {
				setInfo({ version: status.version, download_size: 0 });
				setPhase("ready");
			} else if (status?.last_info) {
				setInfo(status.last_info);
				setPhase("available");
			}
		}).catch(() => {});
	}, []);

	useEffect(() => {
		const unsubAvail = ipcRenderer.on(unloads, "updater.available", (data: UpdateInfo) => {
			setInfo(data);
			setPhase("available");
		});
		const unsubReady = ipcRenderer.on(unloads, "updater.ready", (version: string) => {
			if (!infoRef.current) {
				setInfo({ version, download_size: 0 });
			}
			setPhase("ready");
		});
		const unsubError = ipcRenderer.on(unloads, "updater.error", (msg: string) => {
			setErrorMsg(msg);
			setPhase("error");
		});
		const unsubCancel = ipcRenderer.on(unloads, "updater.cancelled", () => {
			setPhase("available");
		});
		return () => {
			unsubAvail?.();
			unsubReady?.();
			unsubError?.();
			unsubCancel?.();
		};
	}, [unloads]);

	const handleDownload = useCallback(async () => {
		if (!info) return;
		setPhase("downloading");
		try {
			await ipcRenderer.invoke("updater.download", info.version);
		} catch {
			// Error will come via updater.error event
		}
	}, [info]);

	const handleCancel = useCallback(() => {
		ipcRenderer.send("updater.cancel");
	}, []);

	const handleRestart = useCallback(() => {
		if (info) ipcRenderer.send("updater.apply", info.version);
		setInfo(null);
	}, [info]);

	const handleSkip = useCallback(() => {
		if (info) ipcRenderer.send("updater.dismiss", info.version);
		setInfo(null);
	}, [info]);

	const handleClose = useCallback(() => {
		setInfo(null);
	}, []);

	if (!info) return null;

	return (
		<div style={containerStyle}>
			<div style={headerStyle}>
				<div style={titleStyle}>
					<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#31d8ff" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
						<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
						<polyline points="7 10 12 15 17 10" />
						<line x1="12" y1="15" x2="12" y2="3" />
					</svg>
					{phase === "ready" ? "Ready to restart" : `TidaLunar v${info.version} available`}
				</div>
				<button
					onClick={handleClose}
					style={{ background: "none", border: "none", color: "#666", cursor: "pointer", padding: 2, fontSize: 16, lineHeight: 1 }}
				>
					✕
				</button>
			</div>
			<div style={subtitleStyle}>
				{phase === "available" && `Download size: ${formatSize(info.download_size)}`}
				{phase === "downloading" && "Downloading update..."}
				{phase === "ready" && "Update downloaded and ready to install."}
				{phase === "error" && (errorMsg || "Download failed")}
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
				{phase === "error" && (
					<button onClick={handleDownload} style={{ ...btnBase, background: "#eb1e32" }}>
						Retry
					</button>
				)}
			</div>
		</div>
	);
};
