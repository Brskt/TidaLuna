// Patches HTMLMediaElement: TIDAL SDK's <audio> elements for DASH/AAC
// mirror our Rust player's state instead of failing (CEF has no MSE).

import { sendIpc } from "./ipc";
import { applyVolume } from "./volume";
import { attachCoverCanvas, coverSelected, isCoverUrl } from "./video/cover-canvas";
import { productOfStream } from "./sdk-playbackinfo";

// The element Rust's mirror belongs to: the one Shaka plays. Shaka keeps TWO (the active one and
// the one it preloads the next track into), and only the active one may speak for Rust.
let _proxyElement: HTMLMediaElement | null = null;
// An element took over from the owner while Rust still reports the previous track: its events
// are that track's, and none of them belong to the newcomer until Rust loads the next one.
let _handover = false;
// Told when the track ends or Shaka plays its preloaded element: the page's track changed without
// Rust being asked. Called from Shaka's own `play()` and from `proxyEnded`, which a failing
// listener must not break.
let _onHandoff: (() => void) | null = null;
export function onHandoff(cb: () => void) { _onHandoff = cb; }
const handoff = () => {
	try {
		_onHandoff?.();
	} catch (e) {
		console.error("[audio-proxy] handoff listener failed:", e);
	}
};
// Told when the SDK commits a track to the proxied element: a manifest assigned to the element that
// holds Rust's mirror ("src"), or Shaka playing the element it preloaded ("handover"). Called from
// inside the `src` setter and `play()`: a failing listener must not reach them.
export type ProxyBind = "src" | "handover";
let _onBind: ((kind: ProxyBind) => void) | null = null;
export function onProxyBind(cb: (kind: ProxyBind) => void) { _onBind = cb; }
const bound = (kind: ProxyBind) => {
	try {
		_onBind?.(kind);
	} catch (e) {
		console.error("[audio-proxy] bind listener failed:", e);
	}
};
// The stream the SDK holds on the element that carries Rust's mirror.
export function proxyOwnerSrc(): string | null {
	const owner = _proxyElement as any;
	return owner?._lunarProxy ? owner._lunarSrc || null : null;
}
// What the bound element reads as its length. Rust's own is `_rustDuration`: they differ while the
// element holds the stream of another track than the one Rust has.
let _proxyDuration = 0;
// The track Rust holds and its length, as its latest `duration` named them. No product when that
// report named none, or nothing was reported since the track ended.
let _rustProduct: string | undefined;
let _rustDuration = 0;
// Rust's reports belong to the track it plays. The element holds another track's stream when the SDK
// bound one the listener has since left (a load that lost the race to a newer pick): what Rust reports
// is not that stream's, and dispatching it on that element answers the SDK's load with another
// track's length. A stream or a report that names no product cannot be told apart, and is the element's.
const holdsAnotherTrack = () => {
	const held = productOfStream((_proxyElement as any)?._lunarSrc);
	return held !== undefined && _rustProduct !== undefined && held !== _rustProduct;
};
let _proxyTime = 0;
let _proxyPlaying = false;
// Rust has the track the page last loaded into it and has reported its length. TIDAL reloads the
// track it plays (its expired-stream retry on Play, the retry after the network came back) without
// announcing anything. Rust is sent nothing and no `duration` follows, while the SDK's load waits on one.
// A `src` assigned while this holds, for that very track, is such a reload, and answers with the
// length already reported.
let _settled = false;
// What a `src` assigned loads. `reload`: the track Rust has, which Rust's length answers while Rust's
// track is settled. `other`: no answer, a length is Rust's to report.
export type SdkLoad = { kind: "reload" } | { kind: "other" };
// Told by the page, which knows the product of the stream and the one Rust has.
let _classifySdkLoad: ((stream: string) => SdkLoad) | null = null;
export function onSdkLoad(cb: (stream: string) => SdkLoad) { _classifySdkLoad = cb; }
// The page announced a track: Rust's is the previous one's until the load for it reports its length.
export function proxyLoading() { _settled = false; }

// The src of the stream we were told will not play. Held as the src, not a flag: TIDAL
// re-assigns the identical manifest after a failure, and only that one stays refused.
let _failedSrc: string | null = null;

let _selfLoad = false;
export function setSelfLoad(v: boolean) { _selfLoad = v; }
export function isSelfLoad() { return _selfLoad; }

const shouldProxy = (url: string) =>
	typeof url === "string" &&
	(url.includes("audio.tidal.com/mediatracks/") || url.startsWith("data:application/dash+xml"));

const srcDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "src");
console.log("[audio-proxy] srcDesc found:", !!srcDesc, "setter:", !!srcDesc?.set);
if (srcDesc) {
	Object.defineProperty(HTMLMediaElement.prototype, "src", {
		set(value: string) {
			console.log("[audio-proxy] src SET:", typeof value === "string" ? value.slice(0, 80) : value, "isTidal:", shouldProxy(value));
			// Covers bypass `shouldProxy`'s branch: it rebinds `_proxyElement`, `_proxyTime` and
			// `_proxyPlaying` (the AUDIO player's mirror), and a cover there would replace the playing
			// element and reset its time.
			if (isCoverUrl(value)) {
				srcDesc.set!.call(this, value);
				attachCoverCanvas(this, value);
				return;
			}
			if (shouldProxy(value)) {
				(this as any)._lunarProxy = true;
				(this as any)._lunarSrc = value;
				// A manifest on an element while another one holds Rust's mirror is Shaka's preload of
				// the next track into its inactive element: it must not take the mirror from the
				// element that is playing.
				const preload = _proxyElement !== null && _proxyElement !== this;
				if (!preload) {
					_proxyElement = this;
					_handover = false;
					_proxyPlaying = false;
					_proxyTime = 0;

					// A refused stream keeps its verdict while it is the one assigned: announcing
					// readiness again puts the transport back into buffering. Another source
					// gets a fresh verdict.
					if (value === _failedSrc) return;
					_failedSrc = null;
					// Before the readiness burst is queued: a refusal this notification queues runs
					// first (microtasks are FIFO) and the burst below then finds its src refused.
					bound("src");
				}

				// Emit readiness events for the SDK's saga to continue normally.
				// Use microtask timing to match real media element behavior.
				let length = false;
				if (!preload) {
					let load: SdkLoad | undefined;
					try {
						load = _classifySdkLoad?.(value);
					} catch (e) {
						console.error("[audio-proxy] reload probe failed:", e);
					}
					if (load?.kind === "reload" && _settled && !holdsAnotherTrack()) {
						length = true;
						_proxyDuration = _rustDuration;
					}
				}
				const answer = () => {
					if (length) this.dispatchEvent(new Event("durationchange"));
					this.dispatchEvent(new Event("loadedmetadata"));
					this.dispatchEvent(new Event("loadeddata"));
					this.dispatchEvent(new Event("canplay"));
					this.dispatchEvent(new Event("canplaythrough"));
				};
				queueMicrotask(answer);
				return;
			}
			srcDesc.set!.call(this, value);
		},
		get() {
			if ((this as any)._lunarProxy) return (this as any)._lunarSrc || "";
			return srcDesc.get!.call(this);
		},
		configurable: true,
	});
}

const origSetAttribute = Element.prototype.setAttribute;
Element.prototype.setAttribute = function (name: string, value: string) {
	if (this instanceof HTMLMediaElement && name === "src") {
		console.log("[audio-proxy] setAttribute('src'):", typeof value === "string" ? value.slice(0, 80) : value);
		this.src = value;
		return;
	}
	origSetAttribute.call(this, name, value);
};

const OrigAudio = window.Audio;
(window as any).Audio = function (src?: string) {
	const el = new OrigAudio();
	if (src) {
		console.log("[audio-proxy] new Audio(src):", src.slice(0, 80));
		el.src = src;
	}
	return el;
};
(window as any).Audio.prototype = OrigAudio.prototype;

const origPlay = HTMLMediaElement.prototype.play;
HTMLMediaElement.prototype.play = function () {
	console.log("[audio-proxy] play() called, _lunarProxy:", !!(this as any)._lunarProxy);
	if ((this as any)._lunarProxy) {
		if (_proxyElement === null || _proxyElement === this) {
			_proxyElement = this;
			_proxyPlaying = true;
			this.dispatchEvent(new Event("play"));
			sendIpc("player.play");
		} else {
			// Shaka plays its preloaded element: the track changes over to it. Rust has the old
			// track until the page loads the next. This is no command for Rust.
			_proxyElement = this;
			_handover = true;
			_settled = false;
			_proxyPlaying = true;
			_proxyTime = 0;
			this.dispatchEvent(new Event("play"));
			handoff();
			bound("handover");
		}
		return Promise.resolve();
	}
	return origPlay.call(this);
};

const origPause = HTMLMediaElement.prototype.pause;
HTMLMediaElement.prototype.pause = function () {
	if ((this as any)._lunarProxy) {
		// Shaka pauses the outgoing element when it hands over: that is no pause of Rust's track.
		if (_proxyElement !== null && _proxyElement !== this) return;
		_proxyPlaying = false;
		this.dispatchEvent(new Event("pause"));
		sendIpc("player.pause");
		return;
	}
	origPause.call(this);
};

const origLoad = HTMLMediaElement.prototype.load;
HTMLMediaElement.prototype.load = function () {
	if ((this as any)._lunarProxy) {
		// Shaka's unload of a `src=` element: the manifest is gone, and so is the mirror if this
		// element held it. The length stays: a reload of the track Rust is still playing (TIDAL's
		// retry of a prefetched or expired stream on Play) gets no new `duration` report, and the
		// `src` that follows only restarts the clock.
		(this as any)._lunarProxy = false;
		(this as any)._lunarSrc = "";
		if (_proxyElement === this) {
			_proxyElement = null;
			_handover = false;
			_proxyPlaying = false;
			_proxyTime = 0;
		}
		return;
	}

	// TIDAL puts an animated cover on a <source> child, never on the element, and the `src` setter
	// never sees it. `load()` does: a reused element can only change cover through it (the spec
	// ignores a changed `src` on a <source> already inserted). TIDAL calls it from an effect,
	// after the commit, with the child and parent both in the document. Read the child's `src`:
	// `srcset` is ignored on a media child, and `currentSrc` is still stale here.
	const media = this as HTMLMediaElement;
	const candidate = media.querySelector("source")?.src;
	if (candidate && isCoverUrl(candidate)) {
		// The native load still runs: a `<video>` that never loads has no intrinsic size, the frame
		// falls back to 300x150 and the painted cover collapses with it. `attachCoverCanvas` only
		// reads the element once, for the artwork frame it sits in.
		attachCoverCanvas(media, candidate);
	}

	origLoad.call(this);
};

// The hook above stays for its timing (it attaches inside the commit, before TIDAL's unloaded
// `<video>` can be painted), but `load()` is not every selection: a `<source>` inserted into a
// fresh `<video>` starts one on its own. Its `loadstart` is the one signal every selection
// fires. Captured on the document, since media events do not bubble.
document.addEventListener(
	"loadstart",
	(e) => {
		if (e.target instanceof HTMLMediaElement) coverSelected(e.target);
	},
	true,
);

// An element that does not hold the mirror (Shaka's preloaded one) reads as a loaded, idle one: not
// playing, at zero. Its length stays the mirror's, since Shaka's `next()` waits 3 s for a
// `durationchange` on a preloaded element whose `duration` is not a number.
const holdsMirror = (el: any) => _proxyElement === null || _proxyElement === el;
for (const [prop, getter] of Object.entries({
	paused: (el: any) => (holdsMirror(el) ? !_proxyPlaying : true),
	readyState: () => 4, // HAVE_ENOUGH_DATA
	duration: () => _proxyDuration,
	currentTime: (el: any) => (holdsMirror(el) ? _proxyTime : 0),
	buffered: () => ({
		length: 1,
		start: () => 0,
		end: () => _proxyDuration,
	}),
	networkState: () => 2, // NETWORK_LOADING (keeps SDK happy)
	// Null even for a refused stream, deliberately. Shaka reads this property rather than the
	// event, and turns anything non-null into S3016: the one error code TIDAL answers by
	// advancing the queue.
	error: () => null,
}) as [string, (el: any) => any][]) {
	const origDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, prop);
	if (!origDesc) continue;
	Object.defineProperty(HTMLMediaElement.prototype, prop, {
		get() {
			if ((this as any)._lunarProxy) return getter(this);
			return origDesc.get!.call(this);
		},
		set: origDesc.set
			? function (v: any) {
					if ((this as any)._lunarProxy) {
						if (prop === "currentTime" && typeof v === "number" && isFinite(v) && holdsMirror(this)) {
							sendIpc("player.seek", v);
						}
						return;
					}
					origDesc.set!.call(this, v);
				}
			: undefined,
		configurable: true,
	});
}

// Shaka applies TIDAL's volume by writing `mediaElement.volume`, the same way it seeks through
// `currentTime`. The write still lands on the element (shaka reads it back mid-crossfade); on a
// proxied one it is also TIDAL applying a volume, which `applyVolume` turns into Rust's level.
const volumeDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "volume");
if (volumeDesc?.set) {
	Object.defineProperty(HTMLMediaElement.prototype, "volume", {
		get: volumeDesc.get,
		set(v: number) {
			volumeDesc.set!.call(this, v);
			if ((this as any)._lunarProxy) applyVolume();
		},
		configurable: true,
	});
}

// Rust's reports belong to the element only while Rust plays this page's own self-load. Any other track
// (an SDK-native one) is no element's: a manifest Shaka preloaded meanwhile must not read its position,
// since Shaka rewinds a preloaded element it finds past zero, and that write is a seek of Rust's track.
const mirrorsRust = () => !!_proxyElement && !!(_proxyElement as any)._lunarProxy && _selfLoad;

export function proxySetPlaying(playing: boolean) {
	if (!mirrorsRust()) return;
	if (_handover || holdsAnotherTrack()) return;
	_proxyPlaying = playing;
	if (playing) {
		_proxyElement.dispatchEvent(new Event("playing"));
	} else {
		_proxyElement.dispatchEvent(new Event("pause"));
	}
}

export function proxySetTime(time: number) {
	if (!mirrorsRust()) return;
	if (_handover || holdsAnotherTrack()) return;
	_proxyTime = time;
	_proxyElement.dispatchEvent(new Event("timeupdate"));
}

// `product` is the track the length was measured on, when Rust says so.
export function proxySetDuration(duration: number, product?: string) {
	// Kept whether or not an element holds the track yet: Rust measures a load whatever the SDK is doing,
	// and a length reported before the SDK binds the stream (its request slower than ours, or Shaka between
	// its unload and its next `src`) is the answer that bind owes it.
	if (!_selfLoad) return;
	_rustProduct = product;
	_rustDuration = duration;
	_settled = duration > 0;
	if (!mirrorsRust()) return;
	// A handover stays pending too while the element's own track is still to be reported.
	if (holdsAnotherTrack()) return;
	// The first length Rust reports after a handover is the next track's.
	const handedOver = _handover;
	_handover = false;
	_proxyDuration = duration;
	_proxyElement.dispatchEvent(new Event("durationchange"));
	// Shaka played the element before Rust had the track; the play an SDK load ends with went
	// nowhere. A self-loaded stream that is not DASH loads paused and waits for it.
	if (handedOver && _proxyPlaying) sendIpc("player.play");
}

// The track Rust played ended: say so on the element TIDAL observes, as a media element does, then
// read as stopped at the start. The elements keep their binding, since the next track may be one
// TIDAL already loaded into the other element.
// False when nothing it could end is there: no element, one already handed over to the next
// track (its end was the previous one's, and the hand-over already told the listener), or one
// holding another track's stream.
export function proxyEnded(): boolean {
	// Rust's track ended whichever element holds its stream, or none, and Rust holds nothing now. An
	// element that holds another track's stream has nothing to end.
	const ends = !holdsAnotherTrack();
	_rustProduct = undefined;
	_rustDuration = 0;
	_settled = false;
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy) || _handover) return false;
	if (!ends) return false;
	_proxyPlaying = false;
	// Armed before the event: TIDAL's reaction (the next element's prefill, a swap) can come within it.
	handoff();
	_proxyElement.dispatchEvent(new Event("ended"));
	return true;
}

// Retire the loaded stream: stop announcing it ready, and say nothing else.
//
// Withholding the readiness burst is the whole remedy and deliberately the whole scope. Every
// way of telling TIDAL the element failed was measured, and each one advances the queue;
// halting playback without moving it is `playbackControls/STOP`, left to the caller.
export function proxyFail() {
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy)) return;
	_failedSrc = (_proxyElement as any)._lunarSrc ?? null;
	_proxyPlaying = false;
	_settled = false;
}

export function proxyReset() {
	_proxyElement = null;
	_handover = false;
	_settled = false;
	_rustProduct = undefined;
	_rustDuration = 0;
	_proxyPlaying = false;
	_proxyTime = 0;
	_proxyDuration = 0;
}
