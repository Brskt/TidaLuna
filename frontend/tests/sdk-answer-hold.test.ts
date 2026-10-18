// The early runtime's fetch holds the page's answer for an observer that answers a promise, until the
// SDK's playbackinfo answer is recorded (`sdk-playbackinfo.ts`) ahead of the SDK acting on it. The SDK hands
// its native player the stream it parsed from the body in the turns right after `await fetch`, and a
// delegate that names the stream's track (`productOfStream`) reads what was recorded by then.

import { expect, test } from "bun:test";

import { installSdkFetchObserver, observeSdkFetch, productOfStream } from "../src/sdk-playbackinfo";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("fetch_proxy");

const pbi = (id: string) => ({ trackId: Number(id), manifestMimeType: "application/dash+xml", manifest: btoa(`<MPD>hold ${id}</MPD>`) });
const streamOf = (id: string) => `data:application/dash+xml;base64,${pbi(id).manifest}`;
const url = (id: string) => `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=HIGH`;

function pageWith(native: (input: any) => Promise<Response>) {
	const window: any = { fetch: native };
	const slot: any = {};
	runFragment(source, {
		window,
		globalThis: slot,
		invokeIpc: async () => ({ status: 200, headers: {}, body: "{}" }),
		captureToken: () => {},
		_tidalDomain: "tidal.com",
	});
	return { window, slot };
}
const answering = (id: string) => async () => new Response(JSON.stringify(pbi(id)), { status: 200 });

test("a hold that rejects, or a call that rejects, changes nothing for the page", async () => {
	const held = pageWith(answering("7011"));
	held.slot.__lunaFetchTapInstall(() => Promise.reject(new Error("boom")));
	expect((await held.window.fetch(url("7011"))).status).toBe(200);

	const failed = pageWith(async () => {
		throw new TypeError("Failed to fetch");
	});
	failed.slot.__lunaFetchTapInstall(() => Promise.resolve());
	await expect(failed.window.fetch("https://example.com/x")).rejects.toThrow("Failed to fetch");
});

test("the SDK's step after its fetch finds the stream named, though the clone's body completes after its own", async () => {
	// A clone is a tee branch: in a browser the original's body completes first.
	const body = pbi("7012");
	const response = {
		ok: true,
		status: 200,
		json: async () => body,
		clone: () => ({ json: () => new Promise((r) => setTimeout(() => r(body), 20)) }),
	};
	const { window, slot } = pageWith(async () => response as unknown as Response);
	expect(installSdkFetchObserver(slot.__lunaFetchTapInstall)).toBe(true);

	const answer = await window.fetch(url("7012"));
	await answer.json();

	expect(productOfStream(streamOf("7012"))).toBe("7012");
});

test("the observer answers a promise that settles once the answer is recorded, and none for another request", async () => {
	const stream = `data:application/dash+xml;base64,${btoa("<MPD>held</MPD>")}`;
	const answer = Promise.resolve(new Response(JSON.stringify({ trackId: 120, manifestMimeType: "application/dash+xml", manifest: btoa("<MPD>held</MPD>") })));

	const seen = observeSdkFetch(url("120"), answer);
	expect(productOfStream(stream)).toBeUndefined();
	await seen;

	expect(productOfStream(stream)).toBe("120");
	expect(observeSdkFetch("https://api.tidal.com/v1/tracks/120", answer)).toBeUndefined();
});

test("the promise the observer answers never rejects, for an answer it cannot read", async () => {
	const quiet = console.warn;
	console.warn = () => {};
	try {
		await expect(observeSdkFetch(url("121"), Promise.resolve(new Response("<html>")))).resolves.toBeUndefined();
		await expect(observeSdkFetch(url("122"), Promise.reject(new TypeError("Failed to fetch")))).resolves.toBeUndefined();
		await expect(observeSdkFetch(url("123"), Promise.resolve(new Response("{}", { status: 401 })))).resolves.toBeUndefined();
	} finally {
		console.warn = quiet;
	}
});
