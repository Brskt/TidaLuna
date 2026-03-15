// Patches HTMLMediaElement so TIDAL SDK's <audio> elements for DASH/AAC
// mirror our Rust player's state instead of failing (CEF has no MSE).

import { sendIpc } from "./ipc";

let _proxyElement: HTMLMediaElement | null = null;
let _proxyDuration = 0;
let _proxyTime = 0;
let _proxyPlaying = false;

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
			if (shouldProxy(value)) {
				(this as any)._lunarProxy = true;
				(this as any)._lunarSrc = value;
				_proxyElement = this;
				_proxyPlaying = false;
				_proxyTime = 0;

				// Emit readiness events so the SDK's saga continues normally.
				// Use microtask timing to match real media element behavior.
				queueMicrotask(() => {
					this.dispatchEvent(new Event("loadedmetadata"));
					this.dispatchEvent(new Event("loadeddata"));
					this.dispatchEvent(new Event("canplay"));
					this.dispatchEvent(new Event("canplaythrough"));
				});
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
		_proxyPlaying = true;
		this.dispatchEvent(new Event("play"));
		sendIpc("player.play");
		return Promise.resolve();
	}
	return origPlay.call(this);
};

const origPause = HTMLMediaElement.prototype.pause;
HTMLMediaElement.prototype.pause = function () {
	if ((this as any)._lunarProxy) {
		_proxyPlaying = false;
		this.dispatchEvent(new Event("pause"));
		sendIpc("player.pause");
		return;
	}
	origPause.call(this);
};

const origLoad = HTMLMediaElement.prototype.load;
HTMLMediaElement.prototype.load = function () {
	if ((this as any)._lunarProxy) return;
	origLoad.call(this);
};

for (const [prop, getter] of Object.entries({
	paused: () => !_proxyPlaying,
	readyState: () => 4, // HAVE_ENOUGH_DATA
	duration: () => _proxyDuration,
	currentTime: () => _proxyTime,
	buffered: () => ({
		length: 1,
		start: () => 0,
		end: () => _proxyDuration,
	}),
	networkState: () => 2, // NETWORK_LOADING (keeps SDK happy)
	error: () => null,
}) as [string, () => any][]) {
	const origDesc = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, prop);
	if (!origDesc) continue;
	Object.defineProperty(HTMLMediaElement.prototype, prop, {
		get() {
			if ((this as any)._lunarProxy) return getter();
			return origDesc.get!.call(this);
		},
		set: origDesc.set
			? function (v: any) {
					if ((this as any)._lunarProxy) {
						if (prop === "currentTime" && typeof v === "number" && isFinite(v)) {
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

export function proxySetPlaying(playing: boolean) {
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy)) return;
	_proxyPlaying = playing;
	if (playing) {
		_proxyElement.dispatchEvent(new Event("playing"));
	} else {
		_proxyElement.dispatchEvent(new Event("pause"));
	}
}

export function proxySetTime(time: number) {
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy)) return;
	_proxyTime = time;
	_proxyElement.dispatchEvent(new Event("timeupdate"));
}

export function proxySetDuration(duration: number) {
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy)) return;
	_proxyDuration = duration;
	_proxyElement.dispatchEvent(new Event("durationchange"));
}

export function proxyEnded() {
	if (!_proxyElement || !((_proxyElement as any)._lunarProxy)) return;
	_proxyPlaying = false;
	_proxyElement.dispatchEvent(new Event("ended"));
}

export function proxyReset() {
	_proxyElement = null;
	_proxyPlaying = false;
	_proxyTime = 0;
	_proxyDuration = 0;
}
