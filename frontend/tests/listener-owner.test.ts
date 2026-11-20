// Tests `src/plugins/listener-owner.ts` and the card it reports to. A lib registrar hands every
// listener one shared error handler, which logged a plugin's throw under @luna/lib; the plugin's
// own copy of the lib wraps each listener it registers, so the throw reaches that plugin's card.

import { afterEach, expect, mock, test } from "bun:test";
import { registerEmitter } from "@inrixia/helpers";

import { describeError, own, withOwnedListeners } from "../src/plugins/listener-owner";

const ipc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: async (channel: string, ...args: any[]) => {
		if (channel === "plugin.enable" || channel === "plugin.disable") return true;
		return ipc.invokeIpc(channel, ...args);
	},
}));
const { LunaPlugin } = await import("../src/plugins/luna-plugin");

afterEach(() => {
	for (const url of Object.keys(LunaPlugin.plugins)) delete LunaPlugin.plugins[url];
});

type Report = { registrar: string; err: unknown };
const recorder = () => {
	const reports: Report[] = [];
	return { reports, report: (registrar: string, err: unknown) => void reports.push({ registrar, err }) };
};

test("a throwing listener reaches its owner, and the next listener still runs", async () => {
	const { reports, report } = recorder();
	const [onEvent, emit] = registerEmitter<number>();
	const unloads = new Set<() => void>();
	const seen: number[] = [];
	const shared: unknown[] = [];
	onEvent(unloads, own(report, "Test.onEvent", () => {
		throw new Error("boom");
	}));
	onEvent(unloads, (v) => void seen.push(v));

	await emit(1, (err) => shared.push(err));

	expect(seen).toEqual([1]);
	expect(reports.map((r) => [r.registrar, (r.err as Error).message])).toEqual([["Test.onEvent", "boom"]]);
	// Taken by the owner: the shared handler does not log it a second time.
	expect(shared).toEqual([]);
});

test("an async listener's rejection reaches its owner and the emit settles", async () => {
	const { reports, report } = recorder();
	const [onEvent, emit] = registerEmitter<number>();
	onEvent(new Set(), own(report, "Test.onEvent", async () => {
		await Promise.resolve();
		throw new Error("later");
	}));

	await emit(1, () => {});

	expect(reports.map((r) => (r.err as Error).message)).toEqual(["later"]);
});

test("an error whose fields throw is still described, and the emitter carries on", async () => {
	const hostile = {
		get message(): string {
			throw new Error("getter");
		},
		toString(): string {
			throw new Error("toString");
		},
	};
	expect(describeError(hostile)).toBe("an error that cannot be read");
	expect(describeError(new Error("x".repeat(300)))).toHaveLength(203);

	const { reports, report } = recorder();
	const [onEvent, emit] = registerEmitter<number>();
	const seen: number[] = [];
	onEvent(new Set(), own(report, "Test.onEvent", () => {
		throw hostile;
	}));
	onEvent(new Set(), (v) => void seen.push(v));
	await emit(2, () => {});

	expect(seen).toEqual([2]);
	expect(reports).toHaveLength(1);
});

test("what a listener returns passes through, a thenable untouched", () => {
	const { report } = recorder();
	const thenable = { then: () => {} };
	expect(own(report, "r", () => true)()).toBe(true);
	expect(own(report, "r", () => thenable)()).toBe(thenable);
	expect(own(report, "r", (a: number, b: number) => a + b)(2, 3)).toBe(5);
});

// A stand-in for the lib: one class with an emitter static and state written through `this`, an
// interceptor registry as `runInterceptors` reads it, and an IPC listener list.
const makeLib = () => {
	const [onThing, emitThing] = registerEmitter<string>();
	class MediaItem {
		static onPreload = onThing;
		static onMediaTransition = onThing;
		static onPreMediaTransition = onThing;
		static count = 0;
		static bump() {
			this.count += 1;
			return this.count;
		}
	}
	class Plain {
		static onOpen = onThing;
		static onMediaItem = onThing;
		static onScrobble = onThing;
		static onState = onThing;
	}
	const interceptors = new Set<(payload: unknown) => unknown>();
	const ipcListeners: ((...args: unknown[]) => unknown)[] = [];
	const lib = {
		MediaItem,
		ContextMenu: Plain,
		PlayState: Plain,
		other: 1,
		redux: {
			store: "store",
			intercept: (_type: string, _unloads: unknown, cb: (payload: unknown) => unknown) => void interceptors.add(cb),
		},
		ipcRenderer: {
			on: (_unloads: unknown, _channel: string, listener: () => unknown) => void ipcListeners.push(listener),
			once: (_unloads: unknown, _channel: string, listener: () => unknown) => void ipcListeners.push(listener),
			onOpenUrl: (_unloads: unknown, listener: () => unknown) => void ipcListeners.push(listener),
		},
	};
	return { lib, emitThing, interceptors, ipcListeners };
};

test("the copy's registrars wrap listeners; the class behind it stays the shared one", async () => {
	const { lib, emitThing } = makeLib();
	const { reports, report } = recorder();
	const owned = withOwnedListeners(lib, report);

	owned.MediaItem.onMediaTransition(new Set(), () => {
		throw new Error("mine");
	});
	await emitThing("x", () => {});
	expect(reports.map((r) => r.registrar)).toEqual(["MediaItem.onMediaTransition"]);

	// Everything else is the class: statics using `this`, written state, instanceof, subclassing.
	expect(owned.MediaItem.bump()).toBe(1);
	expect(lib.MediaItem.count).toBe(1);
	expect(new lib.MediaItem() instanceof owned.MediaItem).toBe(true);
	class Sub extends owned.MediaItem {}
	expect(new Sub() instanceof lib.MediaItem).toBe(true);
	expect(owned.other).toBe(1);
	expect(owned.redux.store).toBe("store");
});

test("one listener registered twice runs once, as in the lib's own Set", async () => {
	const { lib, emitThing } = makeLib();
	const owned = withOwnedListeners(lib, recorder().report);
	let calls = 0;
	const listener = () => void calls++;
	owned.MediaItem.onPreload(new Set(), listener);
	owned.MediaItem.onPreload(new Set(), listener);
	await emitThing("x", () => {});
	expect(calls).toBe(1);
});

test("a throwing interceptor is reported and cancels nothing; a returned true still cancels", () => {
	const { lib, interceptors } = makeLib();
	const { reports, report } = recorder();
	const owned = withOwnedListeners(lib, report);
	owned.redux.intercept("t", new Set(), () => {
		throw new Error("intercept");
	});
	owned.redux.intercept("t", new Set(), () => true);

	const results = [...interceptors].map((cb) => cb({}));
	expect(results).toEqual([undefined, true]);
	expect(reports.map((r) => r.registrar)).toEqual(["redux.intercept"]);
});

test("IPC listeners are wrapped, onOpenUrl's included", () => {
	const { lib, ipcListeners } = makeLib();
	const { reports, report } = recorder();
	const owned = withOwnedListeners(lib, report);
	const thrower = () => {
		throw new Error("ipc");
	};
	owned.ipcRenderer.on(new Set(), "c", thrower);
	owned.ipcRenderer.once(new Set(), "c", thrower);
	owned.ipcRenderer.onOpenUrl(new Set(), thrower);
	for (const listener of ipcListeners) listener();
	expect(reports.map((r) => r.registrar)).toEqual(["ipcRenderer.on", "ipcRenderer.once", "ipcRenderer.onOpenUrl"]);
});

test("a listener error shows on the card without stopping the plugin, until it restarts", async () => {
	const plugin = LunaPlugin.fromPluginInfo({
		url: "https://x/listener-owner",
		name: "owner",
		manifest: JSON.stringify({ name: "owner" }),
		enabled: true,
		installed: true,
	});
	const logged = console.error;
	console.error = () => {};
	try {
		plugin.reportListenerError("MediaItem.onPreload", new Error("bad"));
	} finally {
		console.error = logged;
	}
	expect(plugin.runtimeError._).toBe("MediaItem.onPreload: bad");
	expect(plugin.running).toBe(true);

	await plugin.disable();
	expect(plugin.runtimeError._).toBeUndefined();

	plugin.runtimeError._ = "stale";
	await plugin.enable();
	expect(plugin.runtimeError._).toBeUndefined();
});
