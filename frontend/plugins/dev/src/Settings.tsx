import { LunaSetting, LunaSettings, LunaSwitch, LunaSwitchSetting } from "@luna/ui";
import { storage, trace } from ".";
import { startReduxLog, stopReduxLog } from "./interceptActions";
import { startIpcLog, stopIpcLog } from "./ipc";

import TextField from "@mui/material/TextField";

import { grey } from "@mui/material/colors";
import React from "react";

export const Settings = () => {
	const [logIpc, setLogIpc] = React.useState(storage.logIpc);

	const [logReduxEvents, setLogReduxEvents] = React.useState(storage.logReduxEvents);
	const [logInterceptsRegex, setLogInterceptsRegex] = React.useState(storage.logInterceptsRegex);

	return (
		<>
			<LunaSettings title="Redux Logging">
				<LunaSetting title="Render redux events" desc="Log Redux dispatch events to console">
					<TextField
						label="Event regex filter"
						placeholder=".*"
						size="small"
						sx={{ flexGrow: 1, marginLeft: 10, marginRight: 10, marginTop: 0.5 }}
						slotProps={{
							htmlInput: { style: { color: grey.A400 } },
						}}
						value={logInterceptsRegex}
						onChange={(e) => {
							setLogInterceptsRegex((storage.logInterceptsRegex = e.target.value));
						}}
					/>
					<LunaSwitch
						tooltip="Render redux events"
						checked={logReduxEvents}
						onChange={async (_, checked) => {
							if (checked)
								await startReduxLog(new RegExp(storage.logInterceptsRegex || ".*"), trace.log.withContext("(Redux)")).catch(
									trace.err.withContext("Failed to start redux event logging").throw,
								);
							else await stopReduxLog().catch(trace.err.withContext("Failed to stop redux event logging").throw);
							setLogReduxEvents((storage.logReduxEvents = checked));
						}}
					/>
				</LunaSetting>
			</LunaSettings>
			<LunaSettings title="IPC Logging" desc="Log cefQuery IPC calls between JS and Rust to console">
				<LunaSwitchSetting
					title="IPC Logging"
					desc={
						<>
							Log <b>JS ↔ Rust</b> IPC calls (cefQuery)
						</>
					}
					checked={logIpc}
					onChange={async (_, checked) => {
						if (checked) startIpcLog();
						else stopIpcLog();
						setLogIpc((storage.logIpc = checked));
					}}
				/>
			</LunaSettings>
		</>
	);
};
