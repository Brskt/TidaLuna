// The clip follows the device chosen in the app: one `setSinkId` at a time on its element, the sink
// taken as applied only once the element says so, a refused one retried only when something changes.

import { afterEach, beforeEach, expect, mock, test } from "bun:test";

const flush = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
const HLS = `data:application/vnd.apple.mpegurl;base64,${btoa("#EXTM3U\n")}`;

class FakeHls {
	static isSupported = () => true;
	static Events = { ERROR: "hlsError" };
	media: HTMLMediaElement | null = null;
	on() {}
	loadSource() {}
	attachMedia(media: HTMLMediaElement) {
		this.media = media;
		media.src = "blob:clip";
	}
	detachMedia() {
		this.media = null;
	}
	startLoad() {}
	destroy() {}
}
mock.module("hls.js/light", () => ({ default: FakeHls }));
mock.module("../src/video/codec-state", () => ({
	codecReady: () => true,
	askCodec: () => Promise.resolve({ ready: true }),
	noteCodecDemand: () => {},
	watchCodecState: () => {},
}));
mock.module("../src/refuse-playback", () => ({
	refusePlayback: () => {},
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

let webDeviceId: string | undefined = "sink-a";
const listeners = new Set<() => void>();
const notifyStore = () => listeners.forEach((l) => l());
mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({
			playbackControls: { mediaProduct: { productId: "1", productType: "video" } },
			player: { activeDeviceId: "dev", availableDevices: [{ id: "dev", webDeviceId }] },
		}),
		subscribe: (cb: () => void) => {
			listeners.add(cb);
			return () => listeners.delete(cb);
		},
		dispatch: () => {},
	},
}));

const media = new EventTarget();
Object.defineProperty(navigator, "mediaDevices", { value: media, configurable: true });

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

const { proxyReset, setSelfLoad } = await import("../src/audio-proxy");
const { releaseClip } = await import("../src/video/clip-driver");

type Call = { sink: string; resolve: () => void; reject: (e: unknown) => void };
let calls: Call[] = [];
const made: HTMLMediaElement[] = [];
const sinkElement = () => {
	const el = document.createElement("video");
	let sinkId = "";
	Object.defineProperty(el, "sinkId", { get: () => sinkId });
	(el as any).setSinkId = (sink: string) =>
		new Promise<void>((resolve, reject) => {
			calls.push({
				sink,
				resolve: () => {
					sinkId = sink;
					resolve();
				},
				reject,
			});
		});
	made.push(el);
	return el;
};
const playClip = async (el: HTMLMediaElement) => {
	el.src = HLS;
	await flush();
	await el.play();
};
const refusal = () => Object.assign(new Error("Requested device not found"), { name: "NotFoundError" });

beforeEach(() => {
	proxyReset();
	setSelfLoad(false);
	calls = [];
	webDeviceId = "sink-a";
	// A refused sink warns; the preload puts `console` back after each test.
	console.warn = () => {};
});

afterEach(() => {
	for (const el of made.splice(0)) releaseClip(el);
	proxyReset();
	setSelfLoad(false);
});

test("the sink is applied once the clip plays, and again only when the choice changes", async () => {
	const el = sinkElement();
	await playClip(el);
	expect(calls.map((c) => c.sink)).toEqual(["sink-a"]);

	// Pending: the element is not on the sink yet, nothing else is asked of it.
	notifyStore();
	expect(calls).toHaveLength(1);

	calls[0]?.resolve();
	await flush();
	notifyStore();
	expect(calls).toHaveLength(1);

	// The clip ends and Shaka plays it again: the element already holds the sink.
	el.dispatchEvent(new Event("ended"));
	el.pause();
	await el.play();
	expect(calls).toHaveLength(1);
});

test("a choice made while one is pending waits for it, and only the last is applied", async () => {
	const el = sinkElement();
	await playClip(el);
	webDeviceId = "sink-b";
	notifyStore();
	webDeviceId = "sink-c";
	notifyStore();
	expect(calls.map((c) => c.sink)).toEqual(["sink-a"]);

	calls[0]?.resolve();
	await flush();
	expect(calls.map((c) => c.sink)).toEqual(["sink-a", "sink-c"]);
});

test("a refused sink is not asked again for the same notifications, but is after a device change", async () => {
	const el = sinkElement();
	await playClip(el);
	calls[0]?.reject(refusal());
	await flush();

	notifyStore();
	notifyStore();
	expect(calls).toHaveLength(1);

	media.dispatchEvent(new Event("devicechange"));
	expect(calls.map((c) => c.sink)).toEqual(["sink-a", "sink-a"]);
	calls[1]?.resolve();
	await flush();
	expect(el.sinkId).toBe("sink-a");
});

test("a refused sink is asked again at the clip's next takeover", async () => {
	const el = sinkElement();
	await playClip(el);
	calls[0]?.reject(refusal());
	await flush();

	// The clip ends, Shaka plays the same element again: a new ownership, a new ask.
	el.dispatchEvent(new Event("ended"));
	el.pause();
	await el.play();
	expect(calls.map((c) => c.sink)).toEqual(["sink-a", "sink-a"]);
});

test("a device change after the clip yielded asks nothing", async () => {
	const el = sinkElement();
	await playClip(el);
	calls[0]?.reject(refusal());
	await flush();
	el.dispatchEvent(new Event("ended"));
	media.dispatchEvent(new Event("devicechange"));
	notifyStore();
	expect(calls).toHaveLength(1);
});

test("a sink refused earlier is asked again once another was chosen and applied in between", async () => {
	webDeviceId = "sink-b";
	const el = sinkElement();
	await playClip(el);
	calls[0]?.reject(refusal());
	await flush();

	webDeviceId = "sink-a";
	notifyStore();
	calls[1]?.resolve();
	await flush();

	webDeviceId = "sink-b";
	notifyStore();
	expect(calls.map((c) => c.sink)).toEqual(["sink-b", "sink-a", "sink-b"]);
});

test("a takeover while a call from the previous one is pending applies the choice made meanwhile", async () => {
	const el = sinkElement();
	await playClip(el);
	el.dispatchEvent(new Event("ended"));
	el.pause();
	await el.play();
	webDeviceId = "sink-b";
	notifyStore();
	expect(calls.map((c) => c.sink)).toEqual(["sink-a"]);

	calls[0]?.resolve();
	await flush();
	expect(calls.map((c) => c.sink)).toEqual(["sink-a", "sink-b"]);
});
