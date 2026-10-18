// Tests the plugins.db part of `pendingRepairsMessage` and `deliverStoreRepair` in
// `src/store-repair.ts`: one pull, one banner, one acknowledgement for every repair owed.

import { expect, test } from "bun:test";

import { deliverStoreRepair, pendingRepairsMessage, pluginsResetMessage, storeRepairMessage, type BannerScreen, type PendingRepairs, type StoreRepair } from "../src/store-repair";

const settings: StoreRepair = { turnedOff: ["video.codec_enabled"], trust: "dropped", saved: true };

test("a plugins reset names what was lost and where the copy is", () => {
	expect(pluginsResetMessage({ asides: ["/d/plugins.db.corrupt-1-0"] })).toBe(
		"TidaLunar replaced its damaged plugin database with an empty one. Your installed plugins and their stored data are gone. A copy of the damaged file is kept as /d/plugins.db.corrupt-1-0.",
	);
});

test("several copies are all named", () => {
	expect(pluginsResetMessage({ asides: ["/d/a", "/d/b"] })).toBe(
		"TidaLunar replaced its damaged plugin database with an empty one. Your installed plugins and their stored data are gone. Copies of the damaged files are kept as /d/a, /d/b.",
	);
});

test("a copy that is gone is not named, the loss still is", () => {
	expect(pluginsResetMessage({ asides: [] })).toBe(
		"TidaLunar replaced its damaged plugin database with an empty one. Your installed plugins and their stored data are gone.",
	);
});

test("a settings repair alone reads as it always did", () => {
	expect(pendingRepairsMessage(settings)).toBe(storeRepairMessage(settings));
});

test("a plugins reset alone carries no word of settings", () => {
	const message = pendingRepairsMessage({ plugins: { asides: ["/d/a"] } });
	expect(message).toBe(pluginsResetMessage({ asides: ["/d/a"] }));
	expect(message).not.toContain("settings");
});

test("both repairs of a launch are one message, the settings part first", () => {
	const pending: PendingRepairs = { ...settings, plugins: { asides: ["/d/a"] } };
	expect(pendingRepairsMessage(pending)).toBe(`${storeRepairMessage(settings)} ${pluginsResetMessage({ asides: ["/d/a"] })}`);
});

test("one banner and one acknowledgement are made for both repairs", async () => {
	const events: string[] = [];
	const shown = new Set<string>();
	const screen: BannerScreen = { shown: (message) => shown.has(message), watch: () => () => {} };
	const store = {
		dispatch: (action: any) => {
			events.push(`banner:${action.payload.message}`);
			shown.add(action.payload.message);
		},
	};
	const pending: PendingRepairs = { ...settings, plugins: { asides: ["/d/a"] } };

	await deliverStoreRepair(store, async () => pending, screen, () => events.push("ack"));

	expect(events).toEqual([`banner:${pendingRepairsMessage(pending)}`, "ack"]);
});
