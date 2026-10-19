// Tests `TidalApi.playbackInfo` (`plugins/lib/src/classes/TidalApi/index.ts`); the self-load a
// replay sends from it is tested in `self-load-fresh-url.test.ts`.
//
// A playbackinfo answer carries SIGNED stream urls that stop working. TIDAL's own player asks for a
// fresh answer on every load; this library asked once per (track, quality) and handed that
// answer back for the rest of the page's life: a track played
// again hours later was loaded from a url that answers 403.
//
// The metadata calls sharing `TidalApi.fetch` are the counterweight: they hold nothing that
// expires, and their memo must stay.

import { beforeEach, expect, mock, test } from "bun:test";

type Reply = { status: number; statusText: string; json: () => Promise<unknown> };

// Every request `tidalFetch` was asked for, and what answers it.
let requests: string[] = [];
let answer: (url: string, nth: number) => Promise<Reply> | Reply = () => ok({});

const ok = (body: unknown): Reply => ({ status: 200, statusText: "OK", json: async () => body });
const status = (code: number): Reply => ({ status: code, statusText: `status ${code}`, json: async () => ({}) });

const realIpc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...realIpc,
	tidalFetch: async (url: string) => {
		requests.push(url);
		return answer(url, requests.filter((u) => u === url).length);
	},
}));
mock.module("../plugins/lib/src/index.safe", () => ({
	libTrace: {
		withSource: () => ({
			trace: { err: { withContext: (context: string) => ({ throw: (message: string) => Promise.reject(new Error(`${context}: ${message}`)) }) } },
		}),
	},
}));
mock.module("../plugins/lib/src/redux", () => ({
	store: { getState: () => ({ session: { clientId: "cid", countryCode: "NZ" }, settings: { language: "en_US" } }) },
}));

const { TidalApi } = await import("../plugins/lib/src/classes/TidalApi");

// A playbackinfo body whose stream url names the request that produced it.
const btsAnswer = (nth: number) => ({
	manifestMimeType: "application/vnd.tidal.bts",
	manifest: btoa(JSON.stringify({ mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "", urls: [`https://cdn/stream-${nth}.mp4`] })),
});
const streamOf = (body: any) => JSON.parse(atob(body.manifest)).urls[0];

let track = 0;
const freshTrack = () => String(++track);

beforeEach(() => {
	requests = [];
	answer = (_url, nth) => ok(btsAnswer(nth));
});

test("a track asked for again is answered with a fresh stream url", async () => {
	const id = freshTrack();
	const first = await TidalApi.playbackInfo(id, "HIGH");
	const replay = await TidalApi.playbackInfo(id, "HIGH");

	expect(requests).toHaveLength(2);
	expect(streamOf(first)).toBe("https://cdn/stream-1.mp4");
	expect(streamOf(replay)).toBe("https://cdn/stream-2.mp4");
});

test("callers asking while the answer is still on its way share one request", async () => {
	const id = freshTrack();
	const gate = Promise.withResolvers<Reply>();
	answer = () => gate.promise;

	const both = Promise.all([TidalApi.playbackInfo(id, "HIGH"), TidalApi.playbackInfo(id, "HIGH")]);
	await new Promise((r) => setTimeout(r, 120));
	gate.resolve(ok(btsAnswer(1)));
	const [a, b] = await both;

	expect(requests).toHaveLength(1);
	expect(a).toBe(b);
});

test("a track TIDAL answers 403 or 404 for stays unavailable without asking again", async () => {
	for (const code of [403, 404]) {
		requests = [];
		const id = freshTrack();
		answer = () => status(code);

		expect(await TidalApi.playbackInfo(id, "HIGH")).toBeUndefined();
		expect(await TidalApi.playbackInfo(id, "HIGH")).toBeUndefined();
		expect(await TidalApi.playbackInfo(id, "LOSSLESS")).toBeUndefined();
		expect(requests, `${code}`).toHaveLength(1);
	}
});

test("a request that failed is asked again", async () => {
	const id = freshTrack();
	answer = (_url, nth) => {
		if (nth === 1) throw new Error("ipc network error");
		return ok(btsAnswer(nth));
	};

	await expect(TidalApi.playbackInfo(id, "HIGH")).rejects.toThrow("ipc network error");
	const retried: any = await TidalApi.playbackInfo(id, "HIGH");
	expect(streamOf(retried)).toBe("https://cdn/stream-2.mp4");
});

test("metadata answers stay memoized", async () => {
	const id = freshTrack();
	answer = () => ok({ id: Number(id), title: "t" });

	const first = await TidalApi.track(id);
	const again = await TidalApi.track(id);

	expect(requests).toHaveLength(1);
	expect(again).toBe(first);
});
