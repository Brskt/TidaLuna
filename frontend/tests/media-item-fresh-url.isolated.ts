// Tests `MediaItem.playbackInfo` and the questions answered from `MediaItem.formatInfo`
// (`plugins/lib/src/classes/MediaItem/MediaItem.ts`): a download started hours after the track's
// format was first asked for must hand Rust urls TIDAL signed for the download, while the format
// questions keep costing one request per quality.
//
// Driven by `media-item-fresh-url.test.ts` in a process of its own, and named without `.test.` to
// keep the suite from collecting it directly: the classes `MediaItem` pulls in are replaced here for
// the whole process, which no other file may inherit.

import { beforeEach, expect, mock, test } from "bun:test";

let requests: string[] = [];
let downloads: { urls: string[] }[] = [];

// A DASH playbackinfo answer; its manifest text names the request that produced it.
const dashAnswer = (nth: number) => ({
	status: 200,
	statusText: "OK",
	json: async () => ({
		manifestMimeType: "application/dash+xml",
		manifest: btoa(`dash-${nth}`),
		audioQuality: "HIGH",
		bitDepth: 16,
		sampleRate: 44100,
	}),
});

const realIpc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...realIpc,
	tidalFetch: async (url: string) => {
		requests.push(url);
		return dashAnswer(requests.length);
	},
	// Rust reads the manifest and answers its urls; here the text it was given names them.
	invokeIpc: async (channel: string, ...args: any[]) => {
		if (channel === "player.parse_dash") {
			const nth = String(args[0]).replace("dash-", "");
			return { initUrl: `https://cdn/init-${nth}.mp4`, segmentUrls: [`https://cdn/seg-${nth}.mp4`], codec: "mp4a.40.2" };
		}
		if (channel === "plugin.download") downloads.push(JSON.parse(args[0]));
	},
}));

const tracer: any = {
	withSource: () => ({ trace: tracer }),
	err: { withContext: () => ({ throw: (m: string) => Promise.reject(new Error(m)) }) },
};
mock.module("@luna/core", () => ({
	ftch: {},
	ReactiveStore: { getStore: () => ({}) },
	BoundedCache: class {},
}));
mock.module("../plugins/lib/src/index.safe", () => ({ libTrace: { withSource: () => ({ trace: tracer }) }, unloads: new Set() }));
const store = { getState: () => ({ session: { clientId: "cid", countryCode: "NZ" }, settings: { language: "en_US" } }) };
mock.module("../plugins/lib/src/redux", () => ({ store, intercept: () => () => {}, replacementTrackId: () => undefined }));
mock.module("../plugins/lib/src/classes/Album", () => ({ Album: class {} }));
mock.module("../plugins/lib/src/classes/Artist", () => ({ Artist: class {} }));
mock.module("../plugins/lib/src/classes/PlayState", () => ({ PlayState: {} }));
mock.module("../plugins/lib/src/classes/ContentBase", () => ({
	ContentBase: class {
		static pinInstance() {}
		static unpinInstance() {}
	},
}));
mock.module("../plugins/lib/src/classes/MediaItem/MediaItem.tags", () => ({ availableTags: {}, makeTags: () => ({}), MetaTags: class {} }));

const { MediaItem } = await import("../plugins/lib/src/classes/MediaItem/MediaItem");
let nextId = 5000;
const freshItem = () => {
	const id = String(++nextId);
	const cache: any = {};
	const item = new MediaItem(id, { item: { id: Number(id), title: "t" } } as any, "track", cache);
	return { item, cache };
};

beforeEach(() => {
	requests = [];
	downloads = [];
});

test("a download after the format ask hands Rust freshly signed urls", async () => {
	const { item } = freshItem();
	expect(await item.fileExtension("HIGH")).toBe("m4a");

	await item.download("/music/a", "HIGH");

	expect(requests).toHaveLength(2);
	expect(downloads).toEqual([expect.objectContaining({ urls: ["https://cdn/init-2.mp4", "https://cdn/seg-2.mp4"] })]);
});

test("the format questions cost one request per quality, however often they are asked", async () => {
	const { item, cache } = freshItem();
	await item.fileExtension("HIGH");
	await item.fileExtension("HIGH");
	await item.fetchBestQuality();

	expect(requests).toHaveLength(1);
	expect(cache.format.HIGH).toEqual({ bitDepth: 16, sampleRate: 44100 });
});

test("a download keeps the format side effects of the ask it makes", async () => {
	const { item, cache } = freshItem();
	await item.download("/music/b", "HIGH");

	expect(requests).toHaveLength(1);
	expect(cache.actualAudioQuality).toBe("HIGH");
	expect(cache.format.HIGH).toEqual({ bitDepth: 16, sampleRate: 44100 });
});
