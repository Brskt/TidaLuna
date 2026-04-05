import type { AnyFn } from "@inrixia/helpers";
import type { LunaUnload } from "./helpers/unloadSet";
import { modules } from "./modules";

// Define getters for core, lib, ui, etc. to proxy to modules
Object.defineProperty(window.luna, "core", {
	get: () => modules["@luna/core"],
	enumerable: true,
	configurable: false,
});
Object.defineProperty(window.luna, "lib", {
	get: () => modules["@luna/lib"],
	enumerable: true,
	configurable: false,
});
Object.defineProperty(window.luna, "ui", {
	get: () => modules["@luna/ui"],
	enumerable: true,
	configurable: false,
});
Object.defineProperty(window.luna, "dev", {
	get: () => modules["@luna/dev"],
	enumerable: true,
	configurable: false,
});
Object.defineProperty(window.luna, "native", {
	get: () => modules["@luna/lib.native"],
	enumerable: true,
	configurable: false,
});
declare global {
	interface Window {
		luna: {
			core?: typeof import("@luna/core");
			lib?: typeof import("@luna/lib");
			ui?: any;
			dev?: any;
		};
	}
	const __platform: string;
	const __TIDALUNAR_VERSION__: string;
	const __ipcRenderer: {
		invoke: (...args: any[]) => Promise<any>;
		send: (...args: any[]) => void;
		on: (channel: string, listener: AnyFn) => LunaUnload;
		once: (channel: string, listener: AnyFn) => LunaUnload;
	};
}
