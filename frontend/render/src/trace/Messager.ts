import { sanitizeData } from "./sanitizeData";

export class Messager {
	private static async dispatch(action: any) {
		// Disgusting hack to avoid cyclic dependencies on init
		// TODO: Remove this monstrosity
		return (await import("../modules")).reduxStore.dispatch(action);
	}
	public static Info(...data: any[]) {
		Messager.dispatch({ type: "message/MESSAGE_INFO", payload: { message: sanitizeData(...data), category: "OTHER", severity: "INFO" } });
	}
	public static Warn(...data: any[]) {
		Messager.dispatch({ type: "message/MESSAGE_WARN", payload: { message: sanitizeData(...data), category: "OTHER", severity: "MESSAGE_WARN" } });
	}
	public static Error(...data: any[]) {
		Messager.dispatch({ type: "message/MESSAGE_ERROR", payload: { message: sanitizeData(...data), category: "OTHER", severity: "ERROR" } });
	}
}
