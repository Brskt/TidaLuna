import { createRequire } from "module";
import { pathToFileURL } from "url";

export const nativeRequire = createRequire(pathToFileURL(process.resourcesPath + "/").href);

import { ipcHandle } from "../ipc";

import * as expose from "../expose";
import { secureLoad } from "./secureLoad";
import { unsafeLoad } from "./unsafeLoad";

declare global {
	var luna: {
		modules: Record<string, any>;
		sendToRender: Electron.WebContents["send"];
		tidalWindow?: Electron.BrowserWindow;
	} & typeof expose;
}
export const luna = (globalThis.luna = {
	modules: {},
	sendToRender: (() => {}) as Electron.WebContents["send"],
	tidalWindow: undefined,
	...expose,
});

// Hot reload - load home page
ipcHandle("__Luna.reload", async () => {
	const win = globalThis.luna.tidalWindow;
	if (!win) return;
	win.loadURL("https://desktop.tidal.com/");
});

ipcHandle("__Luna.registerNative", async (_, fileName: string, code: string) => {
	let exports;
	try {
		exports = secureLoad(fileName, code);
	} catch (err) {
		// Attempt to load depricated modules using unsafe method
		const isDepricatedModule = Error.isError(err) && /Cannot use import statement outside a module/i.test(err.message);
		if (isDepricatedModule) exports = await unsafeLoad(fileName, code);
		else throw err;
	}

	globalThis.luna.modules[fileName] = exports;
	const channel = `__LunaNative.${fileName}`;

	ipcHandle(channel, async (_, exportName, ...args) => {
		try {
			return await exports[exportName](...args);
		} catch (err: any) {
			err.cause = `[Luna.native] (${fileName}).${exportName}`;
			throw err;
		}
	});

	return channel;
});
