// Names the product of a stream the SDK puts on the proxied element. The element carries a manifest
// and no product id, and the SDK's own playbackinfo request is the one place both appear: its URL
// names the product, its answer the stream the SDK then hands to Shaka.
//
// The SDK asks `/tracks/<id>/playbackinfo` when it predicts the native player and
// `/trackManifests/<id>` when it predicts Shaka, and calls `fetch` by its global name on every
// attempt. The page's `fetch` sees each answer. Only the answers are read, from a clone: the SDK's
// body is never consumed, and its answer reaches it unchanged, delayed until the read is recorded.

// What the SDK's `parseManifest` builds the stream from: the BTS manifest's first url, or the DASH
// manifest as a data url (`manifest-parser.ts`, `parseDataUrl` in `playback-info-resolver.ts`).
const DATA_URL = /data:([^;]+);base64,(.+)/;
const BTS_MIME = new Set(["application/vnd.tidal.bts", "application/vnd.tidal.emu"]);

type Endpoint = { pattern: RegExp; streamOf: (body: any) => string | undefined };

const dataUrlOf = (uri: unknown): string | undefined => {
	const match = typeof uri === "string" ? DATA_URL.exec(uri) : null;
	const mime = match?.[1]?.trim();
	const manifest = match?.[2]?.trim();
	return mime && manifest ? `data:${mime};base64,${manifest}` : undefined;
};

const ENDPOINTS: Endpoint[] = [
	{
		pattern: /\/tracks\/(\d+)\/playbackinfo$/,
		streamOf: (body) => {
			if (typeof body?.manifest !== "string" || typeof body?.manifestMimeType !== "string") return undefined;
			if (BTS_MIME.has(body.manifestMimeType)) return JSON.parse(atob(body.manifest)).urls?.[0];
			return `data:${body.manifestMimeType};base64,${body.manifest}`;
		},
	},
	{ pattern: /\/trackManifests\/(\d+)$/, streamOf: (body) => dataUrlOf(body?.data?.attributes?.uri) },
];

// Bounded to the newest 32 answers: the element only needs the load under way and the one before
// it.
const MAX_STREAMS = 32;
const productOfUrl = new Map<string, string>();
const listeners = new Set<() => void>();

// The product the SDK asked for the stream it gave `stream` from, `undefined` when this page never saw
// that answer.
export const productOfStream = (stream: string | null | undefined): string | undefined =>
	stream ? productOfUrl.get(stream) : undefined;

// Told after each answer is recorded: a stream the element already holds may only now be named.
export const onSdkStream = (cb: () => void): (() => void) => {
	listeners.add(cb);
	return () => void listeners.delete(cb);
};

const record = (productId: string, stream: string) => {
	productOfUrl.delete(stream);
	productOfUrl.set(stream, productId);
	if (productOfUrl.size > MAX_STREAMS) productOfUrl.delete(productOfUrl.keys().next().value!);
	console.log("[luna:sdk-pbi] answer for", productId, "stream length", stream.length);
	for (const cb of [...listeners]) {
		try {
			cb();
		} catch (e) {
			console.error("[luna:sdk-pbi] listener failed:", e);
		}
	}
};

const urlOf = (input: unknown): string | undefined => {
	if (typeof input === "string") return input;
	if (input instanceof URL) return input.href;
	return typeof (input as { url?: unknown })?.url === "string" ? (input as { url: string }).url : undefined;
};

const read = async (response: Response, endpoint: Endpoint, productId: string) => {
	const stream = endpoint.streamOf(await response.json());
	if (stream) record(productId, stream);
};

// One `fetch` call of the page, as the early runtime's observer slot hands it over: the request and
// the promise the page gets back. A request that is not the SDK's playbackinfo is ignored, and
// nothing done here can fail or alter the SDK's own call.
//
// Answers a promise, which never rejects, for the answer of the SDK's playbackinfo: it settles once
// that answer is recorded. The early runtime holds the SDK's own promise until then, because the
// clone's body completes after the SDK's own does, and the SDK hands the native player the stream it
// parsed from that body (a delegate that asks `productOfStream` for it) in the turns that follow.
export function observeSdkFetch(input: unknown, answer: Promise<Response>): Promise<void> | undefined {
	try {
		const url = urlOf(input);
		const path = url ? new URL(url, location.href).pathname : "";
		for (const endpoint of ENDPOINTS) {
			const match = endpoint.pattern.exec(path);
			if (!match) continue;
			// Cloned inside the first reaction on the promise, which runs before the SDK's own
			// continuation reads the body.
			return answer
				.then((response) => (response.ok ? read(response.clone(), endpoint, match[1]!) : undefined))
				.catch((e) => console.warn("[luna:sdk-pbi] answer unreadable:", e));
		}
	} catch (e) {
		console.warn("[luna:sdk-pbi] request not tapped:", e);
	}
	return undefined;
}

// `window.fetch` is pinned by the early runtime and ignores assignment; the observer goes to the
// one-shot slot that runtime keeps (`fetch_proxy.js`). A slot that is missing or taken leaves the page
// refusing nothing it did not load itself, and says so here: nothing else would.
export function installSdkFetchObserver(
	install: unknown = (globalThis as { __lunaFetchTapInstall?: unknown }).__lunaFetchTapInstall,
): boolean {
	if (typeof install !== "function") {
		console.warn("[luna:sdk-pbi] no fetch observer slot: the early runtime did not run here; a track the SDK plays and this page did not load is not refused");
		return false;
	}
	if (!install(observeSdkFetch)) {
		console.warn("[luna:sdk-pbi] fetch observer slot already taken; a track the SDK plays and this page did not load is not refused");
		return false;
	}
	return true;
}
