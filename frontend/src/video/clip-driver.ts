// TIDAL's clips: Shaka, blind to MSE, hands `<video>` an HLS `data:` manifest, which a bundled hls.js
// plays there through the transcoding MediaSource. A clip that plays holds the transport: Rust lets
// go of its track and device, and the clip reports its own play state to the OS controls.

import Hls from "hls.js/light";
import { proxyClip, proxyClipGone, setSelfLoad } from "../audio-proxy";
import { updatePlaybackState } from "../controllers/mediasession";
import { sendIpc } from "../ipc";
import { clipSinkId } from "../output-device";
import { refusePlayback } from "../refuse-playback";
import { store } from "../../plugins/lib/src/redux/store";
import { isStoreNotPublished } from "../../plugins/lib/src/redux/storeNotPublished";
import { askCodec, codecReady } from "./codec-state";

const HLS_MANIFEST = "data:application/vnd.apple.mpegurl";

/**
 * How long a clip waits for the codec. Below Shaka's 30 s `streaming.loadTimeout` (`src=` mode, left at
 * its default by TIDAL), whose expiry leaves the spinner; the rest is hls.js's load to `loadedmetadata`.
 */
const CODEC_WAIT_MS = 10_000;

const UNSUPPORTED = "TidaLunar cannot play videos with this version of TIDAL.";
const FAILED = "TidaLunar could not play this video. Please try again or pick another one.";

type ClipState = "playing" | "paused" | "stopped";

type Clip = {
	el: HTMLMediaElement;
	manifest: string;
	hls: Hls | null;
	/** `startLoad` was called: segments are fetched and transcoded from then on. */
	loading: boolean;
	/** Why the clip cannot play, once that is known; refused when it holds the transport. */
	refusal: string | null;
	refused: boolean;
	released: boolean;
	/** Inside hls.js's own attach or detach: its `src` and `load()` writes end nothing. */
	ownWrites: number;
	unlisten: () => void;
};

const clips = new Map<HTMLMediaElement, Clip>();
/** The clip TIDAL plays: Rust has let go of its track, and the clip speaks for the transport. */
let owner: Clip | null = null;
let reported: ClipState | null = null;
let unfollowSink: () => void = () => {};

export function isHlsManifest(value: unknown): value is string {
	return typeof value === "string" && value.startsWith(HLS_MANIFEST);
}

export function clipActive(): boolean {
	return owner !== null;
}

const report = (state: ClipState) => {
	reported = state;
	(window as any).__TL_PLAYING__ = state === "playing";
	updatePlaybackState(state === "playing");
	sendIpc("player.clip_state", state);
};

/**
 * Rust's `state` events are its idle player's while a clip plays: the Stopped answering
 * `player.release` can reach the OS controls after the clip's own report. Said again after each.
 */
export function reassertClipState(): void {
	if (owner && reported) report(reported);
}

// TIDAL names the product before Shaka gets its stream: a clip set while it names a track is Shaka's
// preload of the next item into its inactive element.
const listenerHasVideo = () => {
	try {
		return store.getState().playbackControls?.mediaProduct?.productType === "video";
	} catch (e) {
		if (!isStoreNotPublished(e)) console.warn("[video] the store did not say what plays:", e);
		return true;
	}
};

// The `setSinkId` in flight on an element, whoever asked: Chromium aborts a switch another overtakes.
const sinkInFlight = new WeakMap<HTMLMediaElement, Promise<void>>();

// The clip plays on the device chosen in the app, followed while it holds the transport. The element
// says which sink it is on (`sinkId`, set only once a call resolves); `refused` is the last attempt
// that failed, retried on a device change or the clip's next takeover, forgotten once another is chosen.
const followSink = (clip: Clip): (() => void) => {
	const { el } = clip;
	let refused: string | null = null;
	let warnedUnsupported = false;
	let waiting = false;
	let stopped = false;
	const applyAfter = (call: Promise<void>) => {
		if (waiting) return;
		waiting = true;
		void call.then(() => {
			waiting = false;
			if (!stopped) apply();
		});
	};
	const apply = () => {
		const busy = sinkInFlight.get(el);
		if (busy) return applyAfter(busy);
		let sink: string | null;
		try {
			sink = clipSinkId(store.getState());
		} catch (e) {
			if (!isStoreNotPublished(e)) console.warn("[video] the store did not say which device plays:", e);
			return;
		}
		if (sink !== refused) refused = null;
		if (sink === null || sink === refused || sink === el.sinkId) return;
		if (typeof el.setSinkId !== "function") {
			if (!warnedUnsupported) console.warn("[video] this element cannot choose its device: the clip keeps its output");
			warnedUnsupported = true;
			return;
		}
		const call = el
			.setSinkId(sink)
			.catch((e: unknown) => {
				refused = sink;
				const reason = e instanceof Error ? `${e.name}: ${e.message}` : String(e);
				console.warn("[video] the chosen device was refused, the clip keeps its output until a device change:", reason);
			})
			.finally(() => sinkInFlight.delete(el));
		sinkInFlight.set(el, call);
		applyAfter(call);
	};
	const onDeviceChange = () => {
		refused = null;
		apply();
	};
	let unsubscribe = () => {};
	let devices: MediaDevices | undefined;
	try {
		unsubscribe = store.subscribe(() => {
			if (owner === clip) apply();
		});
	} catch (e) {
		if (!isStoreNotPublished(e)) console.warn("[video] the clip does not follow device changes:", e);
	}
	try {
		devices = navigator.mediaDevices;
		devices?.addEventListener("devicechange", onDeviceChange);
	} catch (e) {
		console.warn("[video] the clip does not follow device arrivals:", e);
	}
	apply();
	return () => {
		stopped = true;
		unsubscribe();
		devices?.removeEventListener("devicechange", onDeviceChange);
	};
};

// Rust lets go of its track and device at once (exclusive and ASIO hold the device otherwise), and
// the last audio track's mirror is dropped: no PLAY, SEEK or device change is routed to Rust.
const takeOver = (clip: Clip) => {
	if (owner === clip) return;
	if (owner) yieldTransport(owner);
	owner = clip;
	sendIpc("player.release");
	setSelfLoad(false);
	proxyClip(clip.el);
	unfollowSink = followSink(clip);
};

const yieldTransport = (clip: Clip) => {
	if (owner !== clip) return;
	owner = null;
	unfollowSink();
	unfollowSink = () => {};
	if (reported === "playing" || reported === "paused") report("stopped");
	reported = null;
};

// Shaka's gapless hand-over pauses the clip without `ended`: the mirror moving to an audio element is
// the only synchronous signal. Silent, Rust's own states speak for that track; the clip stays adopted.
export function mirrorTaken(el: HTMLMediaElement): void {
	if (!owner || owner.el === el) return;
	reported = null;
	yieldTransport(owner);
}

const startLoading = (clip: Clip) => {
	if (!clip.hls || clip.loading) return;
	clip.loading = true;
	clip.hls.startLoad();
};

const release = (clip: Clip) => {
	if (clip.released) return;
	clip.released = true;
	if (clips.get(clip.el) === clip) clips.delete(clip.el);
	clip.unlisten();
	yieldTransport(clip);
	proxyClipGone(clip.el);
	const hls = clip.hls;
	clip.hls = null;
	try {
		hls?.destroy();
	} catch (e) {
		console.warn("[video] hls.js did not shut down cleanly:", e);
	}
};

// Through the banner, never the element's `error`: Shaka turns that into S3016, and TIDAL into a skip.
const refuse = (clip: Clip, message: string) => {
	if (clip.refused) return;
	clip.refused = true;
	release(clip);
	try {
		refusePlayback(message, "");
	} catch (e) {
		console.error("[video] the clip refusal failed:", e);
	}
};

const decline = (clip: Clip, message: string) => {
	clip.refusal = message;
	if (owner === clip) refuse(clip, message);
};

const attach = (clip: Clip) => {
	const hls = new Hls({ enableWorker: false, autoStartLoad: false, capLevelToPlayerSize: true });
	clip.hls = hls;
	// hls.js detaches and attaches the element itself (a reset MediaSource, a new source).
	for (const method of ["attachMedia", "detachMedia"] as const) {
		const own = (hls[method] as (...args: unknown[]) => void).bind(hls);
		(hls as any)[method] = (...args: unknown[]) => {
			clip.ownWrites++;
			try {
				own(...args);
			} finally {
				clip.ownWrites--;
			}
		};
	}
	hls.on(Hls.Events.ERROR, (_event, data) => {
		if (!data.fatal) return;
		// Its type and detail only: the error data carries the signed segment url.
		console.warn("[video] the clip failed:", data.type, data.details);
		decline(clip, FAILED);
	});
	hls.loadSource(clip.manifest);
	hls.attachMedia(clip.el);
};

const settle = async (clip: Clip) => {
	if (!codecReady()) {
		// The backend answers once its step settled: a failed codec is taken again when its schedule
		// allows, and a held one comes back at once with its reason. At the bound: the snapshot's.
		const answer = await askCodec(CODEC_WAIT_MS);
		if (clip.released) return;
		if (!answer.ready) return decline(clip, `TidaLunar cannot play this video: ${answer.refusal}.`);
	}
	// hls.js reads `MediaSource`, ours only once Shaka is blind to MSE.
	if (!Hls.isSupported()) return decline(clip, UNSUPPORTED);
	attach(clip);
	if (owner === clip) startLoading(clip);
};

const listen = (clip: Clip) => {
	const { el } = clip;
	const onPlay = () => {
		// Shaka plays the element it preloaded the clip into, or plays it again after its end.
		takeOver(clip);
		if (clip.refusal !== null) return refuse(clip, clip.refusal);
		startLoading(clip);
		report("playing");
	};
	const onPause = () => {
		if (owner === clip) report("paused");
	};
	const onEnded = () => yieldTransport(clip);
	// Capture: first at the target, ahead of Shaka's listener, which turns a native error into S3016 (a
	// skip). Children's errors cross el in capture too, and are not the clip's.
	const onError = (e: Event) => {
		if (e.target !== el) return;
		e.stopImmediatePropagation();
		console.warn("[video] the clip element failed:", el.error?.code);
		decline(clip, FAILED);
	};
	el.addEventListener("play", onPlay);
	el.addEventListener("pause", onPause);
	el.addEventListener("ended", onEnded);
	el.addEventListener("error", onError, true);
	clip.unlisten = () => {
		el.removeEventListener("play", onPlay);
		el.removeEventListener("pause", onPause);
		el.removeEventListener("ended", onEnded);
		el.removeEventListener("error", onError, true);
	};
};

/**
 * Plays `manifest` on `el` through hls.js. A clip set while TIDAL names another item, or while
 * another clip plays, is Shaka's preload: it fetches nothing until its element plays.
 */
export function adoptClip(el: HTMLMediaElement, manifest: string): void {
	releaseClip(el);
	const proxied = el as any;
	proxied._lunarProxy = false;
	proxied._lunarSrc = "";
	const clip: Clip = {
		el,
		manifest,
		hls: null,
		loading: false,
		refusal: null,
		refused: false,
		released: false,
		ownWrites: 0,
		unlisten: () => {},
	};
	clips.set(el, clip);
	listen(clip);
	const preload = (owner !== null && owner.el !== el) || !listenerHasVideo();
	if (!preload) takeOver(clip);
	void settle(clip);
}

/** Ends the clip on `el`: hls.js lets go of it. A write hls.js makes itself ends nothing. */
export function releaseClip(el: HTMLMediaElement): void {
	const clip = clips.get(el);
	if (!clip || clip.ownWrites > 0) return;
	release(clip);
}
