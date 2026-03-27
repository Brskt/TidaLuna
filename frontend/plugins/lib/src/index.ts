export * from "./index.safe";

export * from "./classes";
export * from "./helpers";

export * as ipcRenderer from "./ipc";
export * as redux from "./redux";

import { StyleTag } from "./classes";
import { observePromise } from "./helpers/observable";

import { unloads } from "./index.safe";

observePromise(unloads, "div[class^='_mainContainer'] > div[class^='_bar'] > div[class^='_title']", 30000).then((title) => {
	if (title !== null) title.innerHTML = 'TIDA<b><span style="color: #32f4ff;">Lunar</span></b>';
});

export { errSignal } from "./index.safe";

// Hide the update notification as it doesn't apply to TidaLunar
new StyleTag("update-fix", unloads, `div div div[data-test="notification-update"]:first-of-type { display: none; }`);
