// A live-reload check still waiting on the source when the user switched live reload off must not
// leave a note: the card would report a failure for a poll that was already stopped.

import { expect, mock, test } from "bun:test";

const OVERTAKEN_URL = "https://x/live-reload-overtaken";

// Only this file's url is answered here; every other call goes to the real `invokeIpc`.
let rejectCheck: ((e: Error) => void) | undefined;
let markAsked!: () => void;
const asked = new Promise<void>((resolve) => (markAsked = resolve));
const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: (channel: string, ...args: any[]) => {
		if (channel === "plugin.check_hash" && args[0] === OVERTAKEN_URL) {
			return new Promise((_, reject) => {
				rejectCheck = reject;
				markAsked();
			});
		}
		return ipc.invokeIpc(channel, ...args);
	},
}));

const { LunaPlugin } = await import("../src/plugins/luna-plugin");

test("a check that fails after live reload was switched off leaves no note", async () => {
	console.error = () => {};
	const plugin = LunaPlugin.fromPluginInfo({ url: OVERTAKEN_URL, name: "Overtaken", manifest: "{}", enabled: true, installed: true });
	const notes: (string | undefined)[] = [];
	plugin.onUpdateNote((note) => notes.push(note));

	plugin.liveReload = true;
	await asked;
	plugin.liveReload = false;
	rejectCheck!(new Error("The source did not answer"));
	await new Promise((resolve) => setTimeout(resolve, 0));

	expect(notes).toStrictEqual([]);
});
