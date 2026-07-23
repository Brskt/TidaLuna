import React, { useCallback, useState } from "react";

import { ipcRenderer } from "@luna/lib";

import {
	LunaButtonSetting,
	LunaSelectItem,
	LunaSelectSetting,
	LunaSettings,
	LunaSwitchSetting,
} from "../../components";

export const LunaLoggingSettings = React.memo(() => {
	const [level, setLevel] = useState(() => Number((window as any).__TIDALUNAR_LOG_LEVEL__ ?? 0));
	const [consoleOn, setConsoleOn] = useState(() => (window as any).__TIDALUNAR_CONSOLE__ === true);

	const handleLevel = useCallback((value: number) => {
		setLevel(value);
		ipcRenderer.send("settings.set_log_level", value);
	}, []);

	const handleConsole = useCallback((_: any, checked: boolean) => {
		setConsoleOn(checked);
		(window as any).__TIDALUNAR_CONSOLE__ = checked;
		ipcRenderer.send("settings.set_console", checked);
	}, []);

	const isWindows = (window as any).__TIDALUNAR_PLATFORM__ === "win32";

	return (
		<LunaSettings title="Logging" desc="Control TidaLunar log verbosity and collect logs.">
			<LunaSelectSetting
				title="Log level"
				desc="How much TidaLunar logs, from low to high. Level 3 is very verbose. Applies immediately."
				value={level}
				onChange={(e) => handleLevel(Number(e.target.value))}
			>
				<LunaSelectItem value={0}>Off</LunaSelectItem>
				<LunaSelectItem value={1}>1 (Low)</LunaSelectItem>
				<LunaSelectItem value={2}>2 (Medium)</LunaSelectItem>
				<LunaSelectItem value={3}>3 (High, most verbose)</LunaSelectItem>
			</LunaSelectSetting>
			{isWindows && (
				<LunaSwitchSetting
					title="Console window"
					desc="Show a console window with live logs (applies on restart)."
					checked={consoleOn}
					onChange={handleConsole}
				/>
			)}
			<LunaButtonSetting
				title="Open logs folder"
				desc="Open the folder of archived logs from past sessions."
				onClick={() => ipcRenderer.send("settings.open_logs_dir")}
			>
				Open
			</LunaButtonSetting>
			<LunaButtonSetting
				title="Open current log file"
				desc="Open this session's live console.log."
				onClick={() => ipcRenderer.send("settings.open_log_file")}
				disabled={level < 1}
			>
				Open
			</LunaButtonSetting>
		</LunaSettings>
	);
});
