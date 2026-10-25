// Tests `deliverStoreRepair` and `bannerScreen` in `src/store-repair.ts`.
//
// The page asks Rust for the repair it owes the listener, banners it, and acknowledges only once
// the banner is on the screen. TIDAL mounts its notification list in the main layout and renders
// a loading overlay instead on `/login`; a banner dispatched there is in the store and not on
// the screen. Anything short of the screen leaves the notice owed.

import { expect, test } from "bun:test";

import { bannerScreen, deliverStoreRepair, storeRepairMessage, type BannerScreen, type StoreRepair } from "../src/store-repair";

const repair: StoreRepair = { turnedOff: ["video.codec_enabled"], trust: "dropped", saved: true };

// TIDAL's `notifications` reducer: an ERROR whose message is already there is dropped. The list
// is on the screen only while `layoutMounted`; mounting renders what the store holds.
const rig = (layoutMounted: boolean) => {
	const events: string[] = [];
	const held: string[] = [];
	const onScreen = new Set<string>();
	const watchers = new Set<() => void>();
	let mounted = layoutMounted;
	const render = () => {
		if (!mounted) return;
		for (const message of held) onScreen.add(message);
		for (const watcher of [...watchers]) watcher();
	};
	const store = {
		dispatch: (action: any) => {
			events.push(`dispatch:${action.type}:${action.payload.category}:${action.payload.severity}`);
			if (!held.includes(action.payload.message)) held.push(action.payload.message);
			render();
		},
	};
	const screen: BannerScreen = {
		shown: (message) => onScreen.has(message),
		watch: (onChange) => {
			watchers.add(onChange);
			return () => watchers.delete(onChange);
		},
	};
	return {
		events,
		store,
		screen,
		watchers,
		acknowledge: () => events.push("ack"),
		navigateOffLogin: () => {
			mounted = true;
			render();
		},
	};
};

const flush = () => new Promise((resolve) => setTimeout(resolve, 10));

test("on a path whose layout is mounted the notice is bannered as an OTHER error, then acknowledged", async () => {
	const { events, store, screen, acknowledge, watchers } = rig(true);
	await deliverStoreRepair(store, async () => repair, screen, acknowledge);
	expect(events).toEqual(["dispatch:message/MESSAGE_ERROR:OTHER:ERROR", "ack"]);
	expect(watchers.size).toBe(0);
});

test("on /login the banner is dispatched, nothing is acknowledged, and the ack follows the navigation", async () => {
	const { events, store, screen, acknowledge, navigateOffLogin, watchers } = rig(false);
	let settled = false;
	deliverStoreRepair(store, async () => repair, screen, acknowledge).then(() => (settled = true));
	await flush();
	expect(events).toEqual(["dispatch:message/MESSAGE_ERROR:OTHER:ERROR"]);
	expect(settled).toBe(false);
	expect(watchers.size).toBe(1);

	navigateOffLogin();
	await flush();
	expect(events).toEqual(["dispatch:message/MESSAGE_ERROR:OTHER:ERROR", "ack"]);
	expect(settled).toBe(true);
	expect(watchers.size).toBe(0);
});

test("a change that does not show the banner acknowledges nothing, and the ack comes once", async () => {
	const { events, store, screen, acknowledge, navigateOffLogin, watchers } = rig(false);
	deliverStoreRepair(store, async () => repair, screen, acknowledge);
	await flush();
	for (const watcher of [...watchers]) watcher();
	await flush();
	expect(events).toEqual(["dispatch:message/MESSAGE_ERROR:OTHER:ERROR"]);
	navigateOffLogin();
	navigateOffLogin();
	await flush();
	expect(events.filter((e) => e === "ack")).toHaveLength(1);
});

test("a banner that throws is not acknowledged", async () => {
	const { events, screen, acknowledge } = rig(true);
	const store = {
		dispatch: () => {
			throw new Error("store down");
		},
	};
	await expect(deliverStoreRepair(store, async () => repair, screen, acknowledge)).rejects.toThrow("store down");
	expect(events).toEqual([]);
});

test("an ask that never answers (a page that navigated) leaves everything untouched", async () => {
	const { events, store, screen, acknowledge } = rig(true);
	let settled = false;
	deliverStoreRepair(store, () => new Promise<StoreRepair | null>(() => {}), screen, acknowledge).then(() => (settled = true));
	await flush();
	expect(settled).toBe(false);
	expect(events).toEqual([]);
});

test("a watch that reports at once is stopped as soon as it returns", async () => {
	const events: string[] = [];
	const screen: BannerScreen = {
		shown: (() => {
			let looks = 0;
			return () => ++looks > 1;
		})(),
		watch: (onChange) => {
			onChange();
			return () => events.push("stop");
		},
	};
	await deliverStoreRepair({ dispatch: () => {} }, async () => repair, screen, () => events.push("ack"));
	expect(events).toEqual(["stop", "ack"]);
});

const fakeDocument = (notifications: { dataTest: string; text: string }[]) => {
	const body = { name: "body" } as unknown as Node;
	const seen: string[] = [];
	const doc = {
		body,
		querySelectorAll: (selector: string) => {
			seen.push(selector);
			return notifications.filter((n) => selector === `[data-test="${n.dataTest}"]`).map((n) => ({ textContent: n.text }));
		},
	} as unknown as Pick<Document, "querySelectorAll" | "body">;
	return { doc, seen };
};

test("bannerScreen reads TIDAL's notification elements and matches the message inside their text", () => {
	const message = storeRepairMessage(repair);
	const { doc, seen } = fakeDocument([{ dataTest: "notification", text: `${message} Close` }]);
	const screen = bannerScreen(doc, () => () => {});
	expect(screen.shown(message)).toBe(true);
	expect(screen.shown("another message")).toBe(false);
	expect(seen[0]).toBe('[data-test="notification"]');
});
