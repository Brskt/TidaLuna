// Tests the plugins.db part of `pendingRepairsMessage` and `deliverStoreRepair` in
// `src/store-repair.ts`: one pull, one banner, one acknowledgement for every repair owed.

import { expect, test } from "bun:test";

import { deliverStoreRepair, pendingRepairsMessage, type BannerScreen, type PendingRepairs, type StoreRepair } from "../src/store-repair";

const settings: StoreRepair = { turnedOff: ["video.codec_enabled"], trust: "dropped", saved: true };

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
