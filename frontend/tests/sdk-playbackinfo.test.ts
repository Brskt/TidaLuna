// Tests for the observer in `src/sdk-playbackinfo.ts`: it names the product of a stream from the
// SDK's own playbackinfo answer, and it must leave the SDK's call exactly as it was. The page's
// `fetch` reaches it through the early runtime's slot, which `early-runtime-fetch.test.ts` runs for
// real; here a stand-in `fetch` hands it the same (input, answer) pair the slot does.

import { expect, spyOn, test } from "bun:test";

import { observeSdkFetch, onSdkStream, productOfStream } from "../src/sdk-playbackinfo";

const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

// A target whose `fetch` answers from `answers`, keyed by the request URL, as the SDK's would, and
// hands each call to the observer as the early runtime's slot does.
const targetOf = (answers: Record<string, () => Response | Promise<Response>>) => {
	const answerOf = async (input: any) => {
		const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
		const answer = answers[url];
		if (!answer) throw new Error(`no answer for ${url}`);
		return answer();
	};
	const target = {
		fetch: ((input: any) => {
			const answer = answerOf(input);
			observeSdkFetch(input, answer);
			return answer;
		}) as typeof fetch,
	};
	return { target };
};

const dashB64 = btoa("<MPD>one</MPD>");
const btsB64 = (urls: string[]) => btoa(JSON.stringify({ mimeType: "audio/mp4", codecs: "mp4a.40.2", keyId: "", urls }));
const LEGACY = (id: string) => `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=HIGH&playbackmode=STREAM&assetpresentation=FULL`;
const MANIFEST = (id: string) => `https://openapi.tidal.com/v2/trackManifests/${id}?adaptive=false&formats=HEAACV1&manifestType=MPEG_DASH`;

test("a legacy answer with a BTS manifest names its first url", async () => {
	const { target } = targetOf({ [LEGACY("101")]: () => json({ trackId: 101, manifestMimeType: "application/vnd.tidal.bts", manifest: btsB64(["https://audio.tidal.com/mediatracks/a.mp4", "https://b"]) }) });
	await target.fetch(LEGACY("101"));
	await flush();

	expect(productOfStream("https://audio.tidal.com/mediatracks/a.mp4")).toBe("101");
});

test("a trackManifests answer names the data url the SDK rebuilds from its uri, trimmed as it trims", async () => {
	const { target } = targetOf({
		[MANIFEST("103")]: () => json({ data: { id: "103", attributes: { uri: ` data:application/dash+xml;base64,${dashB64} `, formats: ["HEAACV1"] } } }),
	});
	await target.fetch(MANIFEST("103"));
	await flush();

	expect(productOfStream(`data:application/dash+xml;base64,${dashB64}`)).toBe("103");
});

test("an answer that is not ok names nothing", async () => {
	const manifest = btoa("<MPD>refused</MPD>");
	const { target } = targetOf({ [LEGACY("105")]: () => json({ trackId: 105, manifestMimeType: "application/dash+xml", manifest }, 401) });
	await target.fetch(LEGACY("105"));
	await flush();

	expect(productOfStream(`data:application/dash+xml;base64,${manifest}`)).toBeUndefined();
});

test("an error body with a 200 status names nothing and fails nobody", async () => {
	const body = { status: 403, subStatus: 4032, userMessage: "no" };
	const { target } = targetOf({ [LEGACY("106")]: () => json(body) });
	const recorded: number[] = [];
	const off = onSdkStream(() => void recorded.push(1));
	const warn = spyOn(console, "warn").mockImplementation(() => {});
	try {
		const response = await target.fetch(LEGACY("106"));
		await flush();

		expect(response.status).toBe(200);
		expect(await response.json()).toEqual(body);
		expect(recorded).toEqual([]);
		expect(warn).not.toHaveBeenCalled();
	} finally {
		off();
		warn.mockRestore();
	}
});

test("a body that is not JSON, or a BTS manifest that is not base64 JSON, names nothing and fails nobody", async () => {
	const warnings: unknown[][] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void warnings.push(args);
	try {
		const { target } = targetOf({
			[LEGACY("107")]: () => new Response("<html>", { status: 200 }),
			[LEGACY("108")]: () => json({ trackId: 108, manifestMimeType: "application/vnd.tidal.bts", manifest: "%%%" }),
		});
		await target.fetch(LEGACY("107"));
		await target.fetch(LEGACY("108"));
		await flush();

		expect(warnings.map((w) => w[0])).toEqual(["[luna:sdk-pbi] answer unreadable:", "[luna:sdk-pbi] answer unreadable:"]);
	} finally {
		console.warn = original;
	}
});

test("a Request and a URL object are read like a string", async () => {
	const request = { trackId: 111, manifestMimeType: "application/dash+xml", manifest: btoa("<MPD>two</MPD>") };
	const url = { trackId: 113, manifestMimeType: "application/dash+xml", manifest: btoa("<MPD>four</MPD>") };
	const { target } = targetOf({ [LEGACY("111")]: () => json(request), [LEGACY("113")]: () => json(url) });

	await target.fetch(new Request(LEGACY("111")));
	await target.fetch(new URL(LEGACY("113")));
	await flush();

	expect(productOfStream(`data:application/dash+xml;base64,${request.manifest}`)).toBe("111");
	expect(productOfStream(`data:application/dash+xml;base64,${url.manifest}`)).toBe("113");
});

test("listeners hear each recorded answer, and one that throws does not stop the others", async () => {
	const heard: string[] = [];
	const quiet = console.error;
	console.error = () => {};
	const off = [
		onSdkStream(() => {
			throw new Error("boom");
		}),
		onSdkStream(() => void heard.push("second")),
	];
	try {
		const { target } = targetOf({ [LEGACY("112")]: () => json({ trackId: 112, manifestMimeType: "application/dash+xml", manifest: btoa("<MPD>three</MPD>") }) });
		await target.fetch(LEGACY("112"));
		await flush();

		expect(heard).toEqual(["second"]);
	} finally {
		console.error = quiet;
		off.forEach((f) => f());
	}
});

