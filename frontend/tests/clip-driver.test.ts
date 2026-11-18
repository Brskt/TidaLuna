// Tests for `src/video/clip-driver.ts`, reached through the `src` setter of `src/audio-proxy.ts`.
//
// TIDAL's Shaka hands a clip to `<video>` as an HLS `data:` manifest: hls.js plays it there, and the
// element stops speaking for Rust until the next audio track, which must find no clip state behind.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const dash = (id: string) => `data:application/dash+xml;base64,${btoa(id)}`;
const HLS = `data:application/vnd.apple.mpegurl;base64,${btoa("#EXTM3U\n")}`;
const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
const channelsFrom = (from: number) => sent.slice(from).map((s) => s.channel);

// hls.js as the driver sees it: `attachMedia` writes its MediaSource url, a detach unloads it.
class FakeHls {
	static instances: FakeHls[] = [];
	static isSupported = () => true;
	static Events = { ERROR: "hlsError" };
	media: HTMLMediaElement | null = null;
	started = 0;
	constructor(readonly config: unknown) {
		FakeHls.instances.push(this);
	}
	on() {}
	loadSource() {}
	attachMedia(media: HTMLMediaElement) {
		this.media = media;
		media.src = `blob:clip-${FakeHls.instances.length}`;
	}
	detachMedia() {
		const media = this.media;
		this.media = null;
		if (!media?.getAttribute("src")?.startsWith("blob:")) return;
		media.removeAttribute("src");
		media.load();
	}
	startLoad() {
		this.started++;
	}
	destroy() {
		this.detachMedia();
	}
}
mock.module("hls.js/light", () => ({ default: FakeHls }));

const codec = { ready: true, bounds: [] as number[], answer: Promise.resolve({ ready: true } as { ready: true } | { ready: false; refusal: string }) };
mock.module("../src/video/codec-state", () => ({
	codecReady: () => codec.ready,
	askCodec: (boundMs: number) => (codec.bounds.push(boundMs), codec.answer),
	noteCodecDemand: () => {},
	watchCodecState: () => {},
}));

const refusals: unknown[][] = [];
mock.module("../src/refuse-playback", () => ({
	refusePlayback: (...args: unknown[]) => void refusals.push(args),
	refusalStands: () => false,
	playOnRefusal: () => "resume",
	askRefusedAgain: () => false,
	refusalDelivered: () => {},
	forgetRefusal: () => {},
}));

mock.module("../src/controllers/mediasession", () => ({
	updatePlaybackState: () => {},
	updateMetadata: () => {},
	setupActionHandlers: () => {},
}));

// What TIDAL's store says the listener has: a clip set while it names a track is Shaka's preload.
let productType = "track";
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({
			playbackControls: { mediaProduct: { productId: "1", productType } },
			player: { activeDeviceId: "speakers", availableDevices: [{ id: "speakers", webDeviceId: "sink-1" }] },
		}),
		subscribe: () => () => {},
		dispatch: () => {},
	},
}));

// Chromium's own `src` setter writes the attribute natively; happy-dom's calls the page's
// `setAttribute`, which audio-proxy routes back to `src`. Native here, as in the app, before
// audio-proxy captures it.
const domSetAttribute = Element.prototype.setAttribute;
Object.defineProperty(HTMLMediaElement.prototype, "src", {
	get(this: HTMLMediaElement) {
		return this.getAttribute("src") ?? "";
	},
	set(this: HTMLMediaElement, value: string) {
		domSetAttribute.call(this, "src", value);
	},
	configurable: true,
});

const { isSelfLoad, proxyOwnerSrc, proxyReset, setSelfLoad } = await import("../src/audio-proxy");
const { clipActive, reassertClipState, releaseClip } = await import("../src/video/clip-driver");

const made: HTMLMediaElement[] = [];
const video = () => {
	const el = document.createElement("video");
	made.push(el);
	return el;
};

beforeEach(() => {
	proxyReset();
	setSelfLoad(false);
	FakeHls.instances = [];
	refusals.length = 0;
	codec.ready = true;
	codec.answer = Promise.resolve({ ready: true });
	productType = "track";
});

afterEach(() => {
	for (const el of made.splice(0)) releaseClip(el);
	proxyReset();
	setSelfLoad(false);
});

test("a clip leaves no audio-proxy state for the next audio track", async () => {
	// Rust plays the first track; Shaka preloads the next into its other element.
	const playing = video();
	const other = video();
	playing.src = dash("audio one");
	other.src = dash("audio two");
	await flush();
	setSelfLoad(true);

	// The next item is a clip, on the element that held the preload, and Shaka plays it.
	other.src = HLS;
	await flush();
	const from = sent.length;
	await other.play();
	expect(channelsFrom(from)).toContain("player.release");
	expect(isSelfLoad()).toBe(false);

	// The audio track after it: the element is proxied again and owns the mirror, which follows
	// Rust only once that track's own load says so.
	other.src = dash("audio three");
	await flush();
	expect(proxyOwnerSrc()).toBe(dash("audio three"));
	expect(isSelfLoad()).toBe(false);
});

test("a clip waits for the codec less than Shaka's 30 s load timeout, and shows the backend's refusal", async () => {
	codec.ready = false;
	codec.bounds.length = 0;
	codec.answer = Promise.resolve({ ready: false, refusal: "the video codec service has stopped" });
	productType = "video";
	const el = video();
	el.src = HLS;
	await flush();
	await flush();

	expect(codec.bounds).toHaveLength(1);
	expect(codec.bounds[0]).toBeLessThan(30_000);
	expect(String(refusals[0]?.[0])).toBe("TidaLunar cannot play this video: the video codec service has stopped.");
	expect(FakeHls.instances).toHaveLength(0);
	expect(el.getAttribute("src")).toBeNull();
});

/** A clip set while the backend has not answered yet; `answer` gives that answer. */
const pendingAsk = async () => {
	codec.ready = false;
	let answer!: (a: { ready: true }) => void;
	codec.answer = new Promise((resolve) => (answer = resolve));
	productType = "video";
	const el = video();
	el.src = HLS;
	await flush();
	return { el, answer };
};

test("a clip waits for the backend's answer, then plays when the codec came", async () => {
	const { answer } = await pendingAsk();
	expect(FakeHls.instances).toHaveLength(0);
	expect(refusals).toHaveLength(0);

	answer({ ready: true });
	await flush();
	expect(FakeHls.instances).toHaveLength(1);
	expect(refusals).toHaveLength(0);
});

test("a clip released while it waits attaches nothing", async () => {
	const { el, answer } = await pendingAsk();
	releaseClip(el);
	answer({ ready: true });
	await flush();
	expect(FakeHls.instances).toHaveLength(0);
});

test("a preloaded clip is not loaded until it plays", async () => {
	const playing = video();
	playing.src = dash("audio one");
	await flush();
	setSelfLoad(true);
	const from = sent.length;

	const preloaded = video();
	preloaded.src = HLS;
	await flush();
	const [hls] = FakeHls.instances;
	expect(hls?.media === preloaded).toBe(true);
	expect(hls?.started).toBe(0);
	expect(channelsFrom(from)).not.toContain("player.release");

	await preloaded.play();
	expect(hls?.started).toBe(1);
	expect(channelsFrom(from)).toContain("player.release");
});

test("a clip holds the transport until its end or a hand-over to an audio track", async () => {
	const states = (from: number) => sent.slice(from).filter((s) => s.channel === "player.clip_state").map((s) => s.args[0]);
	productType = "video";
	const clip = video();
	const audio = video();
	clip.src = HLS;
	await flush();
	await clip.play();
	expect(clipActive()).toBe(true);

	// A plain pause of the clip is the listener's: the clip keeps the transport and says so.
	let from = sent.length;
	clip.pause();
	expect(clipActive()).toBe(true);
	expect(states(from)).toEqual(["paused"]);
	// Rust's Stopped answering the release can reach the OS controls after it: the clip says it again.
	from = sent.length;
	reassertClipState();
	expect(states(from)).toEqual(["paused"]);
	// Its end lets go of the transport, and says so.
	from = sent.length;
	clip.dispatchEvent(new Event("ended"));
	expect(clipActive()).toBe(false);
	expect(states(from)).toEqual(["stopped"]);
	await clip.play();

	// Shaka's gapless hand-over: the audio track it preloaded plays, the clip is paused, never ended.
	from = sent.length;
	audio.src = dash("audio two");
	await flush();
	await audio.play();
	clip.pause();
	setSelfLoad(true);
	reassertClipState();
	expect(clipActive()).toBe(false);
	expect(states(from)).toEqual([]);

	// The mirror going to a fresh element after a reset lets go of the clip as well.
	const next = video();
	const other = video();
	next.src = HLS;
	await flush();
	await next.play();
	expect(clipActive()).toBe(true);
	proxyReset();
	other.src = dash("audio three");
	await flush();
	await other.play();
	expect(clipActive()).toBe(false);
});

const FAILED = "TidaLunar could not play this video. Please try again or pick another one.";

test("a native error on a playing clip is refused, never reaches Shaka", async () => {
	productType = "video";
	const el = video();
	// Shaka's listener on the element: registered at attach, before the clip's src is set.
	let shakaSaw = 0;
	el.addEventListener("error", () => void shakaSaw++);
	el.src = HLS;
	await flush();
	await el.play();
	const [hls] = FakeHls.instances;

	el.dispatchEvent(new Event("error"));

	expect(shakaSaw).toBe(0);
	expect(refusals).toHaveLength(1);
	expect(refusals[0]?.[0]).toBe(FAILED);
	expect(hls?.media).toBeNull();
	expect(clipActive()).toBe(false);
});

test("a native error on a preloaded clip refuses it once, when it plays", async () => {
	const playing = video();
	playing.src = dash("audio one");
	await flush();
	setSelfLoad(true);
	const preloaded = video();
	let shakaSaw = 0;
	preloaded.addEventListener("error", () => void shakaSaw++);
	preloaded.src = HLS;
	await flush();

	// Twice. The clip is not released until it plays: the second error is declined again.
	preloaded.dispatchEvent(new Event("error"));
	preloaded.dispatchEvent(new Event("error"));
	expect(shakaSaw).toBe(0);
	expect(refusals).toHaveLength(0);

	await preloaded.play();
	expect(refusals).toHaveLength(1);
	expect(refusals[0]?.[0]).toBe(FAILED);
});
