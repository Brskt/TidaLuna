// Tests `TidalApi.lyrics` (`plugins/lib/src/classes/TidalApi/index.ts`). The v1 API answers 404 for
// the lyrics of a track the user uploaded, and lyrics plugins and the download tags got nothing; the
// OpenAPI has them, in another shape the v1 one is rebuilt from.

import { beforeEach, expect, mock, test } from "bun:test";

type Reply = { status: number; statusText: string; json: () => Promise<unknown> };

let requests: string[] = [];
let answer: (url: string) => Reply = () => notFound;

const ok = (body: unknown): Reply => ({ status: 200, statusText: "OK", json: async () => body });
const notFound: Reply = { status: 404, statusText: "Not Found", json: async () => ({}) };

const realIpc = await import("../src/ipc");
mock.module("../src/ipc", () => ({
	...realIpc,
	tidalFetch: async (url: string) => {
		requests.push(url);
		return answer(url);
	},
}));
mock.module("../plugins/lib/src/index.safe", () => ({
	libTrace: {
		withSource: () => ({
			// `throw` throws where it is called, as createLogger's does (render/src/trace/createLogger.ts)
			trace: {
				err: {
					withContext: (context: string) => ({
						throw: (message: string) => {
							throw new Error(`${context}: ${message}`);
						},
					}),
				},
			},
		}),
	},
}));
mock.module("../plugins/lib/src/redux", () => ({
	store: { getState: () => ({ session: { clientId: "cid", countryCode: "NZ" }, settings: { language: "en_US" } }) },
}));

const { TidalApi } = await import("../plugins/lib/src/classes/TidalApi");

// `fetch` memoizes by url: each test asks about a track no other test asked about.
let track = 0;
const freshTrack = () => String(++track);
const isV1 = (url: string) => url.startsWith("https://desktop.tidal.com/v1/tracks/");
const isOpenApi = (url: string) => url.startsWith("https://openapi.tidal.com/v2/tracks/");

beforeEach(() => {
	requests = [];
	answer = () => notFound;
});

test("an uploaded track's lyrics come from the OpenAPI, in the v1 shape", async () => {
	const id = freshTrack();
	answer = (url) =>
		isOpenApi(url)
			? ok({
					included: [
						{ id: "t", type: "tracks", attributes: {} },
						{
							id: "l",
							type: "lyrics",
							attributes: {
								text: "words",
								lrcText: "[00:01.00] words",
								direction: "RIGHT_TO_LEFT",
								provider: { source: "THIRD_PARTY", name: "Musixmatch", commonTrackId: "c1", lyricsId: "l1" },
							},
						},
					],
				})
			: notFound;

	expect(await TidalApi.lyrics(id)).toEqual({
		trackId: id,
		lyricsProvider: "Musixmatch",
		providerCommontrackId: "c1",
		providerLyricsId: "l1",
		lyrics: "words",
		subtitles: "[00:01.00] words",
		isRightToLeft: true,
	});
	expect(requests.map((url) => (isV1(url) ? "v1" : isOpenApi(url) ? "openapi" : url))).toEqual(["v1", "openapi"]);
	expect(requests[1]).toContain("include=lyrics");
});

test("lyrics the v1 API has are not asked of the OpenAPI", async () => {
	const id = freshTrack();
	const v1 = { trackId: id, lyricsProvider: "p", providerCommontrackId: "c", providerLyricsId: "l", lyrics: "words", isRightToLeft: false };
	answer = (url) => (isV1(url) ? ok(v1) : notFound);

	expect(await TidalApi.lyrics(id)).toEqual(v1);
	expect(requests).toHaveLength(1);
});

test("a track with lyrics on neither API has none", async () => {
	const id = freshTrack();
	answer = (url) => (isOpenApi(url) ? ok({ included: [{ id: "l", type: "lyrics", attributes: { technicalStatus: "NOT_FOUND" } }] }) : notFound);
	expect(await TidalApi.lyrics(id)).toBeUndefined();

	const unknown = freshTrack();
	answer = () => notFound;
	expect(await TidalApi.lyrics(unknown)).toBeUndefined();
});

test("an OpenAPI failure reaches the caller instead of reading as no lyrics", async () => {
	const id = freshTrack();
	answer = (url) => (isOpenApi(url) ? { status: 500, statusText: "Server Error", json: async () => ({}) } : notFound);

	await expect(TidalApi.lyrics(id)).rejects.toThrow("500 Server Error");

	// The failure is not kept: the next ask goes to the OpenAPI again.
	answer = (url) => (isOpenApi(url) ? ok({ included: [{ id: "l", type: "lyrics", attributes: { text: "words" } }] }) : notFound);
	expect((await TidalApi.lyrics(id))?.lyrics).toBe("words");
});
