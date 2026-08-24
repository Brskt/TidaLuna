// Tests for `src/audio-proxy.ts`.
//
// The module patches `HTMLMediaElement.prototype` as it loads, so importing it is the setup;
// `tests/setup.ts` (preloaded) has already put a DOM and a `cefQuery` stub in place.
//
// Every test uses a distinct src. The proxy's state is module-level by design (it models the
// one element TIDAL plays through), and a fresh src retires the previous verdict: distinct
// sources isolate the tests the way two real tracks do.

import { expect, test } from "bun:test";

import { proxyFail } from "../src/audio-proxy";

const READINESS = ["loadedmetadata", "loadeddata", "canplay", "canplaythrough"];

// A DASH manifest data URI, which is what `shouldProxy` matches on.
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;

// Lets the proxy's `queueMicrotask` readiness burst run.
const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

// An `<audio>` element already armed with `src`, past its readiness burst.
async function armed(id: string) {
	const el = document.createElement("audio");
	const seen: string[] = [];
	for (const name of [...READINESS, "error"]) {
		el.addEventListener(name, () => seen.push(name));
	}
	el.src = dash(id);
	await flush();
	return { el, seen };
}

test("a healthy stream still announces itself", async () => {
	// The guard on the nominal path: refusing a stream must not cost the four events TIDAL's
	// saga waits on, nor the readiness a Rust-backed element is entitled to claim.
	const { el, seen } = await armed("healthy");
	expect(seen).toEqual(READINESS);
	expect(el.readyState).toBe(4);
	expect(el.networkState).toBe(2);
	expect(el.error).toBeNull();
});

test("a refusal leaves shaka nothing to mint an error from", async () => {
	// Read out of TIDAL's shipped bundle. Shaka turns a media element that looks broken into
	// error code S3016, the one code its saga answers by advancing the queue. A refused
	// element must therefore report what a healthy one does, `error` null above all: shaka
	// reads the property, not the event.
	const { el, seen } = await armed("refused");
	proxyFail();
	await flush();
	expect(el.error).toBeNull();
	expect(el.readyState).toBe(4);
	expect(el.networkState).toBe(2);
	expect(seen.filter((n) => n === "error")).toEqual([]);
});

test("re-arming the same source does not resurrect readiness", async () => {
	// This is the whole of what `proxyFail` may do. After a refusal TIDAL re-assigns the
	// identical manifest, and a proxy that answers with a fresh readiness burst puts the
	// transport straight back into loading: the spinner that never stops. Stopping playback
	// is said with `playbackControls/STOP` instead, which TIDAL's own error handler uses and
	// which its reducer never lets touch the queue.
	const { el } = await armed("sticky");
	proxyFail();

	const after: string[] = [];
	for (const name of READINESS) el.addEventListener(name, () => after.push(name));
	el.src = dash("sticky");
	await flush();

	expect(after).toEqual([]);
});

test("a different source gets a fresh verdict", async () => {
	// A refusal belongs to the stream that earned it. The next track must not inherit it, or
	// one undecodable song would mute the rest of the queue.
	await armed("verdict-old");
	proxyFail();

	const next = await armed("verdict-new");
	expect(next.seen).toEqual(READINESS);
	expect(next.el.readyState).toBe(4);
});
