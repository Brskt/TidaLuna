// Tests `early-runtime/fetch_proxy.js`, which pins `window.fetch` for the whole page. Run for
// real, on a stand-in `window` and `globalThis`: the pins it makes (non-configurable by design)
// land on an object this file throws away rather than on the one every other test shares.
//
// The pin is why the observer slot exists: the accessor's setter swallows every assignment. A
// host that wrapped `window.fetch` by assigning to it was installed and blind, with nothing failing.

import { expect, test } from "bun:test";

import { installSdkFetchObserver, productOfStream } from "../src/sdk-playbackinfo";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("fetch_proxy");

const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
};
const pbi = (id: string) => ({ trackId: Number(id), manifestMimeType: "application/dash+xml", manifest: btoa(`<MPD>fragment ${id}</MPD>`) });
const streamOf = (id: string) => `data:application/dash+xml;base64,${pbi(id).manifest}`;
const url = (id: string) => `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=HIGH`;

type Page = { window: any; slot: any; proxied: string[]; captured: string[] };

// The fragment on a page whose native `fetch` answers `native(input)`. `proxy.fetch` answers with `proxyBody`.
function pageWith(native: (input: any) => Promise<Response>, proxyBody = "{}"): Page {
	const window: any = { fetch: native };
	const fakeGlobal: any = {};
	const proxied: string[] = [];
	const captured: string[] = [];
	runFragment(source, {
		window,
		globalThis: fakeGlobal,
		invokeIpc: async (channel: string, target: string) => {
			proxied.push(`${channel} ${target}`);
			return { status: 200, headers: {}, body: proxyBody };
		},
		captureToken: (token: string) => captured.push(token),
		_tidalDomain: "tidal.com",
	});
	return { window, slot: fakeGlobal, proxied, captured };
}

const answering = (id: string) => async () => new Response(JSON.stringify(pbi(id)), { status: 200 });

const warnings = async (run: () => unknown | Promise<unknown>) => {
	const seen: unknown[][] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void seen.push(args);
	try {
		await run();
	} finally {
		console.warn = original;
	}
	return seen;
};

test("window.fetch is pinned: a non-configurable accessor whose setter ignores assignment", () => {
	const { window } = pageWith(answering("1"));
	const before = window.fetch;
	const desc = Object.getOwnPropertyDescriptor(window, "fetch")!;

	expect(typeof desc.get).toBe("function");
	expect(desc.configurable).toBe(false);
	window.fetch = () => Promise.reject(new Error("replaced"));
	expect(window.fetch).toBe(before);
	expect(() => Object.defineProperty(window, "fetch", { value: () => {} })).toThrow(TypeError);
});

test("the observer slot is pinned on the global: not writable, not configurable, not enumerable", () => {
	const { slot } = pageWith(answering("2"));
	const desc = Object.getOwnPropertyDescriptor(slot, "__lunaFetchTapInstall")!;

	expect(typeof desc.value).toBe("function");
	expect(desc.writable).toBe(false);
	expect(desc.configurable).toBe(false);
	expect(desc.enumerable).toBe(false);
	expect(Reflect.set(slot, "__lunaFetchTapInstall", () => true)).toBe(false);
	expect(() => Object.defineProperty(slot, "__lunaFetchTapInstall", { value: () => true })).toThrow(TypeError);
});

test("the slot is one-shot: a second caller is refused, and so is anything that is not a function", () => {
	const { slot } = pageWith(answering("3"));
	const install = slot.__lunaFetchTapInstall;
	const first = () => {};

	expect(install("not a function")).toBe(false);
	expect(install(undefined)).toBe(false);
	// The refusals above took nothing: the host's observer still gets in.
	expect(install(first)).toBe(true);
	expect(install(() => {})).toBe(false);
	expect(install(first)).toBe(false);
});

test("the observer sees each call's request and the very promise the page gets back", async () => {
	const { window, slot } = pageWith(answering("4"));
	const seen: { input: unknown; answer: Promise<Response> }[] = [];
	slot.__lunaFetchTapInstall((input: unknown, answer: Promise<Response>) => void seen.push({ input, answer }));

	const returned = window.fetch(url("4"));

	expect(seen).toHaveLength(1);
	expect(seen[0]!.input).toBe(url("4"));
	expect(seen[0]!.answer).toBe(returned);
	expect((await returned).status).toBe(200);
});

test("the slot's observer names the product of an answer, and the SDK still reads its body whole", async () => {
	const { window, slot } = pageWith(async (input: any) => new Response(JSON.stringify(pbi(/tracks\/(\d+)\//.exec(typeof input === "string" ? input : input.url)![1]!)), { status: 200 }));
	expect(installSdkFetchObserver(slot.__lunaFetchTapInstall)).toBe(true);

	const viaString = await window.fetch(url("7002"));
	// openapi-fetch hands `fetch` a lone Request.
	const viaRequest = await window.fetch(new Request(url("7003")));
	await flush();

	expect(await viaString.json()).toEqual(pbi("7002"));
	expect((await viaRequest.json()).trackId).toBe(7003);
	expect(productOfStream(streamOf("7002"))).toBe("7002");
	expect(productOfStream(streamOf("7003"))).toBe("7003");
});

test("an answer the proxy fallback produced is observed too", async () => {
	const { window, slot, proxied } = pageWith(async () => {
		throw new TypeError("Failed to fetch");
	}, JSON.stringify(pbi("7004")));
	expect(installSdkFetchObserver(slot.__lunaFetchTapInstall)).toBe(true);

	const response = await window.fetch(url("7004"));
	await flush();

	expect(proxied).toEqual([`proxy.fetch ${url("7004")}`]);
	expect((await response.json()).trackId).toBe(7004);
	expect(productOfStream(streamOf("7004"))).toBe("7004");
});

test("a rejected call rejects the page's promise as it did, and the observer is told", async () => {
	const { window, slot } = pageWith(async () => {
		throw new TypeError("Failed to fetch");
	});
	let told: Promise<Response> | undefined;
	slot.__lunaFetchTapInstall((_input: unknown, answer: Promise<Response>) => {
		told = answer;
	});

	// Not a tidal.com url: the proxy fallback keeps out of it and the native error stands.
	await expect(window.fetch("https://example.com/x")).rejects.toThrow("Failed to fetch");
	await expect(told).rejects.toThrow("Failed to fetch");
});

test("an observer that throws cannot fail the page's fetch, and is reported", async () => {
	const { window, slot } = pageWith(answering("7005"));
	slot.__lunaFetchTapInstall(() => {
		throw new Error("boom");
	});

	let response: Response | undefined;
	const reported = await warnings(async () => {
		response = await window.fetch(url("7005"));
	});

	expect(response!.status).toBe(200);
	expect(reported.map((w) => w[0])).toEqual(["[luna:fetch] observer failed:"]);
});

test("with no observer installed the page's fetch is the pinned one and nothing else", async () => {
	const { window, captured } = pageWith(answering("7006"));

	const response = await window.fetch("https://api.tidal.com/v1/x", { headers: { Authorization: "Bearer luna_nonce" } });

	expect(response.status).toBe(200);
	expect(captured).toEqual(["luna_nonce"]);
});

test("the early runtime's own work on a call runs whether or not an observer is installed", async () => {
	const { window, slot, captured } = pageWith(answering("7007"));
	slot.__lunaFetchTapInstall(() => {});

	await window.fetch("https://api.tidal.com/v1/x", { headers: { Authorization: "Bearer luna_other" } });

	expect(captured).toEqual(["luna_other"]);
});

test("installing from a page without the early runtime says so, and leaves the page unobserved", async () => {
	const reported = await warnings(() => {
		expect(installSdkFetchObserver(undefined)).toBe(false);
	});

	expect(reported).toHaveLength(1);
	expect(String(reported[0]![0])).toContain("no fetch observer slot");
});

test("installing when the slot is taken says so", async () => {
	const { slot } = pageWith(answering("7008"));
	slot.__lunaFetchTapInstall(() => {});

	const reported = await warnings(() => {
		expect(installSdkFetchObserver(slot.__lunaFetchTapInstall)).toBe(false);
	});

	expect(reported).toHaveLength(1);
	expect(String(reported[0]![0])).toContain("slot already taken");
});
