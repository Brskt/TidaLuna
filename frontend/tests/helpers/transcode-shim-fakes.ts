// The real MediaSource, WebCodecs and `luna-cover://` bodies the transcoding shim runs over in tests
// (happy-dom has none). The MediaSource follows media-source-2: synchronous `updating`, events as
// tasks, the spec's exceptions.

import { afterAll, afterEach, beforeEach } from "bun:test";

export type RealAppend = { bytes: Uint8Array; offset: number };
/** Every real append, remove, abort, `changeType` and `endOfStream`, in order. Emptied before each test. */
export const log: string[] = [];
const task = (fn: () => void) => setTimeout(fn, 0);
const dom = (name: string, msg = name) => new DOMException(msg, name);

export class FakeSourceBuffer extends EventTarget {
	updating = false;
	removed = false;
	private offset = 0;
	private appendMode = "segments";
	private windowStart = 0;
	private windowEnd = Infinity;
	readonly appends: RealAppend[] = [];
	readonly buffered = {
		length: 0,
		start() {
			throw dom("IndexSizeError");
		},
		end() {
			throw dom("IndexSizeError");
		},
	};
	readonly removes: [number, number][] = [];
	readonly changeTypes: string[] = [];
	/** While true an operation stays in flight until `release()`. */
	hold = false;
	token = 0;
	private release_: (() => void) | null = null;
	constructor(
		readonly type: string,
		readonly ms: FakeMediaSource,
	) {
		super();
	}
	fire(type: string) {
		this.dispatchEvent(new Event(type));
	}
	private guard() {
		if (this.removed) throw dom("InvalidStateError", "removed");
		if (this.updating) throw dom("InvalidStateError", "This SourceBuffer is still processing an operation.");
	}
	// An `ended` MediaSource opens again on an append, a remove or a setter, as the spec has it.
	private reopen() {
		if (this.ms.readyState === "ended") {
			this.ms.readyState = "open";
			task(() => this.ms.dispatchEvent(new Event("sourceopen")));
		}
	}
	get timestampOffset() {
		return this.offset;
	}
	set timestampOffset(value: number) {
		this.guard();
		this.reopen();
		this.offset = value;
	}
	get mode() {
		return this.appendMode;
	}
	set mode(value: string) {
		this.guard();
		this.reopen();
		this.appendMode = value;
	}
	get appendWindowStart() {
		return this.windowStart;
	}
	set appendWindowStart(value: number) {
		this.guard();
		this.windowStart = value;
	}
	get appendWindowEnd() {
		return this.windowEnd;
	}
	set appendWindowEnd(value: number) {
		this.guard();
		this.windowEnd = value;
	}
	private begin() {
		this.updating = true;
		const token = ++this.token;
		task(() => this.fire("updatestart"));
		const finish = () => {
			if (this.token !== token) return;
			this.updating = false;
			task(() => this.fire("update"));
			task(() => this.fire("updateend"));
		};
		if (this.hold) this.release_ = finish;
		else task(finish);
	}
	release() {
		const release = this.release_;
		this.release_ = null;
		release?.();
	}
	appendBuffer(data: Uint8Array) {
		this.guard();
		this.reopen();
		log.push("append");
		this.appends.push({ bytes: data, offset: this.offset });
		this.begin();
	}
	abort() {
		if (this.removed) throw dom("InvalidStateError", "removed");
		if (this.ms.readyState !== "open") throw dom("InvalidStateError", "not open");
		if (this.updating) {
			this.token++;
			this.release_ = null;
			this.updating = false;
			log.push("real-abort");
			task(() => this.fire("abort"));
			task(() => this.fire("updateend"));
		}
		this.windowStart = 0;
		this.windowEnd = Infinity;
	}
	remove(start: number, end: number) {
		if (this.removed) throw dom("InvalidStateError", "removed");
		const duration = this.ms.duration;
		if (Number.isNaN(duration)) throw new TypeError("duration NaN");
		if (start < 0 || start > duration) throw new TypeError("bad start");
		if (!(end > start)) throw new TypeError("bad end");
		if (this.updating) throw dom("InvalidStateError", "This SourceBuffer is still processing an operation.");
		this.reopen();
		log.push("remove");
		this.removes.push([start, end]);
		this.begin();
	}
	changeType(type: string) {
		this.guard();
		this.reopen();
		log.push("changeType");
		this.changeTypes.push(type);
	}
}

export class FakeMediaSource extends EventTarget {
	static isTypeSupported = (type: string) => /vp09|opus/.test(type);
	readonly sourceBuffers: FakeSourceBuffer[] = [];
	readyState = "closed";
	private length = NaN;
	get duration() {
		return this.length;
	}
	set duration(value: number) {
		if (this.readyState !== "open") throw dom("InvalidStateError", "duration: not open");
		if (this.sourceBuffers.some((sb) => sb.updating)) throw dom("InvalidStateError", "duration: updating");
		this.length = value;
	}
	setLiveSeekableRange() {}
	open() {
		this.readyState = "open";
		this.dispatchEvent(new Event("sourceopen"));
	}
	addSourceBuffer(type: string) {
		const sb = new FakeSourceBuffer(type, this);
		this.sourceBuffers.push(sb);
		return sb;
	}
	removeSourceBuffer(sb: FakeSourceBuffer) {
		const i = this.sourceBuffers.indexOf(sb);
		if (i < 0) throw dom("NotFoundError", "not a child");
		if (sb.updating) {
			sb.token++;
			sb.updating = false;
			task(() => sb.fire("abort"));
			task(() => sb.fire("updateend"));
		}
		sb.removed = true;
		this.sourceBuffers.splice(i, 1);
	}
	endOfStream() {
		if (this.readyState !== "open") throw dom("InvalidStateError", "endOfStream: not open");
		if (this.sourceBuffers.some((sb) => sb.updating)) throw dom("InvalidStateError", "endOfStream: updating");
		log.push("eos");
		this.readyState = "ended";
		task(() => this.dispatchEvent(new Event("sourceended")));
	}
}

const FAKED = ["MediaSource", "VideoEncoder", "AudioEncoder", "VideoFrame", "AudioData", "fetch"];

/**
 * The shim over the fake MediaSource, with the calling file's hooks. The shim captures the real
 * MediaSource as its module loads, as bootstrap's first import does: hence a dynamic import, once.
 */
export async function loadShimOverFakes() {
	const realBefore = Object.getOwnPropertyDescriptor(globalThis, "MediaSource");
	(globalThis as any).MediaSource = FakeMediaSource;
	const shim = await import("../../src/video/transcode-shim");
	const { watchCodecState } = await import("../../src/video/codec-state");
	watchCodecState();
	afterAll(() => {
		if (realBefore) Object.defineProperty(globalThis, "MediaSource", realBefore);
		else delete (globalThis as any).MediaSource;
	});
	let saved = new Map<string, PropertyDescriptor | undefined>();
	beforeEach(() => {
		saved = new Map(FAKED.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
		log.length = 0;
	});
	afterEach(() => {
		for (const [name, was] of saved) {
			if (was) Object.defineProperty(globalThis, name, was);
			else delete (globalThis as any)[name];
		}
	});
	return shim;
}

let seq = 0;
export const codec = (kind: string) =>
	(window as any).__LUNAR_IPC_EMIT__("settings.video_codec_state", { seq: ++seq, enabled: true, phase: { kind } });

/** `hold`: the Opus encoder keeps its latest packet until `flush()`, as Chromium's holds its last packets. */
export const fakeWebCodecs = ({ hold = false } = {}) => {
	const seen = {
		audioData: [] as { timestamp: number; duration: number }[],
		videoFrames: [] as { timestamp: number; key: boolean }[],
		audioEncoders: 0,
		encoders: [] as { state: string }[],
	};
	(globalThis as any).VideoFrame = class {
		timestamp: number;
		constructor(_data: BufferSource, init: VideoFrameBufferInit) {
			this.timestamp = init.timestamp;
		}
		close() {}
	};
	(globalThis as any).VideoEncoder = class {
		encodeQueueSize = 0;
		state = "unconfigured";
		constructor(private init: VideoEncoderInit) {
			seen.encoders.push(this);
		}
		configure() {
			this.state = "configured";
		}
		encode(frame: { timestamp: number }, opts?: VideoEncoderEncodeOptions) {
			seen.videoFrames.push({ timestamp: frame.timestamp, key: !!opts?.keyFrame });
			const chunk = { timestamp: frame.timestamp, type: opts?.keyFrame ? "key" : "delta", byteLength: 1, copyTo: (into: Uint8Array) => (into[0] = 7) };
			this.init.output(chunk as unknown as EncodedVideoChunk, {});
		}
		async flush() {}
		close() {
			this.state = "closed";
		}
		addEventListener() {}
	};
	(globalThis as any).AudioData = class {
		timestamp: number;
		duration: number;
		constructor(init: AudioDataInit) {
			this.timestamp = init.timestamp;
			this.duration = Math.round((init.numberOfFrames / init.sampleRate) * 1e6);
			seen.audioData.push({ timestamp: this.timestamp, duration: this.duration });
		}
		close() {}
	};
	// OpusHead with a pre-skip of 312, carried by the first packet's metadata as Chromium does.
	const opusHead = new Uint8Array([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 1, 2, 0x38, 0x01, 0x44, 0xac, 0, 0, 0, 0, 0]);
	(globalThis as any).AudioEncoder = class {
		encodeQueueSize = 0;
		state = "unconfigured";
		private first = true;
		private held: { timestamp: number; duration: number } | null = null;
		constructor(private init: AudioEncoderInit) {
			seen.audioEncoders++;
			seen.encoders.push(this);
		}
		configure() {
			this.state = "configured";
		}
		private emit(data: { timestamp: number; duration: number }) {
			const chunk = { timestamp: data.timestamp, duration: data.duration, type: "key", byteLength: 1, copyTo: (into: Uint8Array) => (into[0] = 9) };
			const meta = this.first ? { decoderConfig: { codec: "opus", sampleRate: 44100, numberOfChannels: 2, description: opusHead } } : {};
			this.first = false;
			this.init.output(chunk as unknown as EncodedAudioChunk, meta);
		}
		encode(data: { timestamp: number; duration: number }) {
			if (!hold) return this.emit(data);
			if (this.held) this.emit(this.held);
			this.held = { timestamp: data.timestamp, duration: data.duration };
		}
		async flush() {
			if (this.held) this.emit(this.held);
			this.held = null;
		}
		close() {
			this.state = "closed";
		}
		addEventListener() {}
	};
	return seen;
};

export const concat = (parts: Uint8Array[]) => {
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
};

/** An `LVF1` body: Rust's magic, then the records as `src/video/fragment/decode.rs` writes them. */
export const lvf1 = (...records: Uint8Array[]) => concat([new TextEncoder().encode("LVF1"), ...records]);

/** One 2x2 I420 frame. */
export const videoRecord = (pts: number, duration: number) => {
	const head = new DataView(new ArrayBuffer(25));
	head.setUint8(0, 0);
	head.setUint32(1, 90000, true);
	head.setBigInt64(5, BigInt(pts), true);
	head.setUint32(13, duration, true);
	head.setUint32(17, 2, true);
	head.setUint32(21, 2, true);
	return concat([new Uint8Array(head.buffer), new Uint8Array(6)]);
};

/** One AAC frame's PCM, stereo at 44.1 kHz. */
export const audioRecord = (pts: number) => {
	const frames = 1024;
	const head = new DataView(new ArrayBuffer(29));
	head.setUint8(0, 1);
	head.setUint32(1, 44100, true);
	head.setBigInt64(5, BigInt(pts), true);
	head.setUint32(13, frames, true);
	head.setUint32(17, 44100, true);
	head.setUint32(21, 2, true);
	head.setUint32(25, frames, true);
	return concat([new Uint8Array(head.buffer), new Uint8Array(frames * 2 * 4)]);
};

export const serve = (body: Uint8Array<ArrayBuffer>) => {
	globalThis.fetch = (async () => new Response(body)) as unknown as typeof fetch;
};
