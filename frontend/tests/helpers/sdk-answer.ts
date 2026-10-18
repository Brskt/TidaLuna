// The playbackinfo answers the SDK receives, handed to the observer the way the page's `fetch`
// does (`src/sdk-playbackinfo.ts`). Afterwards the stream the SDK hands to Shaka names its product.

import { observeSdkFetch } from "../../src/sdk-playbackinfo";

// A DASH answer for `id`, awaited until the observer has taken it in.
export async function sdkDashAnswer(id: string): Promise<void> {
	const url = `https://api.tidal.com/v1/tracks/${id}/playbackinfo?audioquality=HIGH`;
	observeSdkFetch(url, Promise.resolve(new Response(JSON.stringify({ trackId: 1, manifestMimeType: "application/dash+xml", manifest: btoa(id) }))));
	for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
}

// The LOSSLESS answer that carried `stream`, as the early runtime's slot hands it over.
export const nameStream = (stream: string, productId: string) =>
	observeSdkFetch(
		`https://api.tidal.com/v1/tracks/${productId}/playbackinfo?audioquality=LOSSLESS`,
		Promise.resolve(new Response(JSON.stringify({ trackId: Number(productId), manifestMimeType: "application/vnd.tidal.bts", manifest: btoa(JSON.stringify({ urls: [stream] })) }))),
	);
