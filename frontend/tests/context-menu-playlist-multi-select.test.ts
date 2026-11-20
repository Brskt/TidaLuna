// Tests `ContextMenu.onMediaItem` (`plugins/lib/src/classes/ContextMenu.ts`). A multi-selection inside
// a playlist opens its menu with `contextMenu/OPEN_PLAYLIST_ITEM_MULTI_SELECT`, which onMediaItem did
// not intercept: plugin buttons were missing from that menu.

import { expect, mock, test } from "bun:test";

const interceptors = new Map<string, (payload: unknown) => unknown>();
const menu = { tag: "context-menu" };
const fromTMediaItemsCalls: unknown[] = [];
const collection = { tag: "collection" };

const tracer = { withContext: () => () => undefined };
mock.module("../plugins/lib/src/index.safe", () => ({
	libTrace: { withSource: () => ({ trace: { err: tracer, msg: { err: tracer } } }) },
	unloads: new Set(),
}));
mock.module("../plugins/lib/src/redux", () => ({
	intercept: (type: string, _unloads: unknown, cb: (payload: unknown) => unknown) => void interceptors.set(type, cb),
}));
mock.module("../plugins/lib/src/helpers", () => ({ observePromise: async () => menu }));
mock.module("../plugins/lib/src/classes/Album", () => ({ Album: {} }));
mock.module("../plugins/lib/src/classes/Playlist", () => ({ Playlist: {} }));
mock.module("../plugins/lib/src/classes/MediaItem", () => ({
	MediaItems: {
		fromIds: () => collection,
		fromTMediaItems: (items: unknown) => {
			fromTMediaItemsCalls.push(items);
			return collection;
		},
	},
}));

const { ContextMenu } = await import("../plugins/lib/src/classes/ContextMenu");

test("a playlist multi-selection reaches onMediaItem with its tracks and videos", async () => {
	const received: unknown[] = [];
	ContextMenu.onMediaItem(new Set(), (event) => void received.push(event));

	const open = interceptors.get("contextMenu/OPEN_PLAYLIST_ITEM_MULTI_SELECT");
	expect(open).toBeDefined();
	await open!({
		items: [
			{ id: 11, meta: { itemId: "a" }, type: "tracks" },
			{ id: 12, meta: { itemId: "b" }, type: "videos" },
			{ id: "13", meta: { itemId: "c" }, type: "tracks" },
		],
		position: [0, 0],
		sourceContext: {},
	});

	expect(fromTMediaItemsCalls).toEqual([
		[
			{ item: { id: 11 }, type: "track" },
			{ item: { id: 12 }, type: "video" },
			{ item: { id: "13" }, type: "track" },
		],
	]);
	expect(received).toEqual([{ mediaCollection: collection, contextMenu: menu }]);
});
