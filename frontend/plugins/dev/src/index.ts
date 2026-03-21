import { ReactiveStore, Tracer, type LunaUnload } from "@luna/core";
import { startReduxLog, stopReduxLog } from "./interceptActions";
import { startIpcLog, stopIpcLog } from "./ipc";

export const { trace, errSignal } = Tracer("[DevTools]");

export const unloads = new Set<LunaUnload>();
unloads.add(stopIpcLog);
unloads.add(stopReduxLog);

export const storage = await ReactiveStore.getPluginStorage("DevTools", {
	logIpc: false,
	logReduxEvents: false,
	logInterceptsRegex: ".*",
});

if (storage.logIpc) startIpcLog();
if (storage.logReduxEvents)
	startReduxLog(new RegExp(storage.logInterceptsRegex || ".*"), trace.log.withContext("(Redux)")).catch(
		trace.err.withContext("Failed to start redux event logging").throw,
	);

export { Settings } from "./Settings";
