// The page's MediaSource once Shaka is blind: its SourceBuffers take H.264 + AAC-LC fMP4, Rust
// decodes each append and WebCodecs re-encodes it to VP9 + Opus for Chromium's real SourceBuffer.
// hls.js takes the events as facts: a second `updateend` drops its next append, a missing one stalls.

import { codecReady, noteCodecDemand } from "./codec-state";
import { BT709, encoderConfig } from "./cover-encode";
import { rustDeadlineMs } from "./deadlines";
import { boxesIn, contiguous, decodeSpans, initOf, type Span, trexDurations } from "./fmp4-reader";
import { type FragmentSample, type TrackInit, writeFragment, writeInit } from "./fmp4-writer";
import { type AudioRecord, decodeFragment, type VideoRecord } from "./fragment-client";
import { RealMediaSource } from "./real-media-source";

const VIDEO_CODEC = "vp09.00.40.08";
const AUDIO_CODEC = "opus";
const VIDEO_TRACK = 1;
const AUDIO_TRACK = 2;

/** Frames queued in the video encoder before the decode loop waits for it. */
const MAX_QUEUED = 8;

/** Opus bitrate of the re-encoded clip audio. */
const OPUS_BITRATE = 128_000;

/** Per-fragment cap: decode and re-encode run in series, and a 1080p fragment takes seconds. Rust
 * stops its decode at the same deadline; it is read from Rust when the shim installs. */
let watchdogMs = 0;

const EMPTY = new Uint8Array(0);

const parseMime = (type: string) => {
	const [container = "", ...params] = type.split(";").map((p) => p.trim());
	const codecs = params.find((p) => p.toLowerCase().startsWith("codecs="));
	return {
		container: container.toLowerCase(),
		codecs: (codecs?.slice("codecs=".length) ?? "")
			.replace(/"/g, "")
			.split(",")
			.map((c) => c.trim().toLowerCase())
			.filter(Boolean),
	};
};

const isAvc1 = (codec: string) => codec === "avc1" || codec.startsWith("avc1.");
const isAacLc = (codec: string) => codec === "mp4a.40.2";

/** Refused whatever the real MediaSource says: Rust decodes none of them, and a yes would fail later. */
const REFUSED = [/^avc3/, /^hvc1/, /^hev1/, /^mp4a\.40\.(5|29)$/];

/** fMP4 whose every codec Rust decodes: H.264 in `avc1`, AAC-LC. */
export function transcodable(type: string): boolean {
	const { container, codecs } = parseMime(type);
	return (container === "video/mp4" || container === "audio/mp4") && codecs.length > 0 && codecs.every((c) => isAvc1(c) || isAacLc(c));
}

export function shimIsTypeSupported(type: string): boolean {
	if (transcodable(type)) {
		if (codecReady()) return true;
		// A player told no is demand: a codec whose download failed is fetched again.
		noteCodecDemand();
		return false;
	}
	if (parseMime(type).codecs.some((c) => REFUSED.some((r) => r.test(c)))) return false;
	return RealMediaSource?.isTypeSupported(type) ?? false;
}

type Kinds = { video: boolean; audio: boolean };

const kindsOf = (type: string): Kinds => {
	const { codecs } = parseMime(type);
	return { video: codecs.some(isAvc1), audio: codecs.some(isAacLc) };
};

const outputType = ({ video, audio }: Kinds) =>
	video ? `video/mp4; codecs="${audio ? `${VIDEO_CODEC},${AUDIO_CODEC}` : VIDEO_CODEC}"` : `audio/mp4; codecs="${AUDIO_CODEC}"`;

const concat = (parts: Uint8Array[]) => {
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
};

/** The pre-skip of an OpusHead (`description` of Chromium's first Opus packet), little-endian. */
const opusPreSkip = (description: AllowSharedBufferSource) => {
	const bytes = ArrayBuffer.isView(description)
		? new Uint8Array(description.buffer, description.byteOffset, description.byteLength)
		: new Uint8Array(description);
	if (bytes.length < 12 || String.fromCharCode(...bytes.subarray(0, 8)) !== "OpusHead") return undefined;
	return bytes[10]! | (bytes[11]! << 8);
};

const closeCodec = (codec?: { state: string; close(): void }) => {
	if (codec && codec.state !== "closed") codec.close();
};

const waitDequeue = (codec: EventTarget, signal: AbortSignal) =>
	new Promise((resolve) => {
		codec.addEventListener("dequeue", resolve, { once: true });
		signal.addEventListener("abort", resolve, { once: true });
	});

type EncodedPacket = { timestamp: number; duration: number | null; key: boolean; data: Uint8Array };

const copyChunk = (chunk: EncodedVideoChunk | EncodedAudioChunk): EncodedPacket => {
	const data = new Uint8Array(chunk.byteLength);
	chunk.copyTo(data);
	return { timestamp: chunk.timestamp, duration: chunk.duration, key: chunk.type === "key", data };
};

type VideoState = { encoder: VideoEncoder; width: number; height: number; chunks: EncodedPacket[]; error?: unknown };

/** One persistent Opus encoder: a fresh one per fragment would prime again at each boundary. */
type AudioState = {
	encoder: AudioEncoder;
	sampleRate: number;
	channels: number;
	timescale: number;
	/** Where the next PCM record starts when it follows on, in `timescale`. */
	next: number;
	packets: EncodedPacket[];
	preSkip?: number;
	/** The end of the last packet appended, in `timescale`: the next packet starts there. */
	end?: number;
	error?: unknown;
	generation: number;
};

/** PCM that continues the stretch `audio` encodes: same format, starting within one frame of its end. */
const follows = (audio: AudioState, record: AudioRecord) =>
	audio.sampleRate === record.sampleRate &&
	audio.channels === record.channels &&
	audio.timescale === record.timescale &&
	Math.abs(record.pts - audio.next) <= record.duration;

/** `packets` as one fragment's samples, chained from where `audio`'s last appended packet ended. */
const opusOutput = (audio: AudioState, packets: EncodedPacket[]): Output | null => {
	const first = packets[0];
	if (!first) return null;
	const ticks = (micros: number) => Math.round((micros * audio.timescale) / 1e6);
	const base = audio.end ?? ticks(first.timestamp);
	let start = base;
	const samples = packets.map((packet) => {
		if (typeof packet.duration !== "number") throw new Error("an Opus packet carries no duration");
		// Chained from the last packet's end: rounding each on its own would drift.
		const end = ticks(packet.timestamp + packet.duration);
		const sample = { data: packet.data, key: true, duration: end - start };
		start = end;
		return sample;
	});
	audio.end = start;
	return { base, samples };
};

type VideoTrack = Extract<TrackInit, { kind: "video" }>;

/** This append's frames, timed by the input: each output chunk finds its pts by its timestamp. */
type VideoFragment = { track: VideoTrack | null; ticks: Map<number, number>; end: number };

type Output = { base: number; samples: FragmentSample[] };

/** An append of ours on the real SourceBuffer, until its `updateend`. */
type Own = { failed: boolean; aborted: boolean; done: Promise<boolean>; settle: (failed: boolean) => void };

type Outcome = "update" | "error" | "abort";

const SOURCE_BUFFER_EVENTS = ["updatestart", "update", "updateend", "error", "abort"] as const;

class TranscodingSourceBuffer extends EventTarget {
	onupdatestart: ((event: Event) => void) | null = null;
	onupdate: ((event: Event) => void) | null = null;
	onupdateend: ((event: Event) => void) | null = null;
	onerror: ((event: Event) => void) | null = null;
	onabort: ((event: Event) => void) | null = null;

	private op: AbortController | null = null;
	private own: Own | null = null;
	private offset = 0;
	private removed = false;
	private passthrough = false;
	private init = EMPTY;
	private last: { media: Uint8Array; spans: Map<number, Span> } | null = null;
	private video: VideoState | null = null;
	private audio: AudioState | null = null;
	private generation = 0;
	private videoTrack: VideoTrack | null = null;
	/** The output init the real SourceBuffer holds; a different one is appended first. */
	private written = "";
	private seq = 0;
	/** An end-of-stream drain in flight; any operation of the page's own cancels it. */
	private drain: AbortController | null = null;
	private draining: Promise<boolean> | null = null;

	constructor(
		readonly real: SourceBuffer,
		private kinds: Kinds,
	) {
		super();
		for (const type of SOURCE_BUFFER_EVENTS) real.addEventListener(type, this.onReal);
	}

	/** The page's own operations only: the real SourceBuffer is also busy with an append of ours. */
	get updating() {
		return this.op !== null || (this.real.updating && !this.ownInFlight);
	}
	private get ownInFlight() {
		return this.own !== null && !this.own.aborted;
	}
	get buffered() {
		return this.real.buffered;
	}
	get mode() {
		return this.real.mode;
	}
	set mode(mode: AppendMode) {
		this.abortOwn();
		this.real.mode = mode;
	}
	get appendWindowStart() {
		return this.real.appendWindowStart;
	}
	set appendWindowStart(time: number) {
		this.abortOwn();
		this.real.appendWindowStart = time;
	}
	get appendWindowEnd() {
		return this.real.appendWindowEnd;
	}
	set appendWindowEnd(time: number) {
		this.abortOwn();
		this.real.appendWindowEnd = time;
	}
	/** Held until the real append it applies to: the input's own fragment is not appended at all. */
	get timestampOffset() {
		return this.offset;
	}
	set timestampOffset(offset: number) {
		this.offset = offset;
	}

	/** An Opus encoder is open, and may hold the last packets of what it was given. */
	get holdsAudio() {
		return this.audio !== null;
	}

	appendBuffer(data: BufferSource) {
		this.usable();
		// A transcoded append queues behind the drain's; a passthrough one has no such lane.
		if (this.passthrough) this.supersede();
		else this.cancelDrain();
		// Copied now, as Chromium does: the caller may reuse its buffer once this returns.
		const bytes = ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice() : new Uint8Array(data).slice();
		if (this.passthrough) {
			this.real.timestampOffset = this.offset;
			this.real.appendBuffer(bytes);
			return;
		}
		const op = new AbortController();
		this.op = op;
		void this.run(op, bytes);
	}

	abort() {
		if (this.removed) throw new DOMException("This SourceBuffer has been removed from its MediaSource.", "InvalidStateError");
		this.supersede();
		if (this.op) this.end(this.op, "abort");
		this.real.abort();
	}

	remove(start: number, end: number) {
		this.usable();
		this.cancelDrain();
		if (!this.ownInFlight) return this.real.remove(start, end);
		// Behind the drain's append, on the shim's own events: the page's `updating` is already true.
		const op = new AbortController();
		this.op = op;
		void this.runRemove(op, start, end);
	}

	private async runRemove(op: AbortController, start: number, end: number) {
		await Promise.resolve();
		if (this.op !== op) return;
		this.emit("updatestart");
		try {
			await this.realRun(() => this.real.remove(start, end), op.signal);
			this.end(op, "update");
		} catch {
			this.end(op, "error");
		}
	}

	changeType(type: string) {
		this.usable();
		this.supersede();
		const kinds = transcodable(type) ? kindsOf(type) : null;
		// `avc1` to `avc1`: the output stays VP9, and the next init says what changed.
		if (kinds && !this.passthrough && kinds.video === this.kinds.video && kinds.audio === this.kinds.audio) return;
		this.real.changeType(kinds ? outputType(kinds) : type);
		this.passthrough = !kinds;
		if (kinds) this.kinds = kinds;
		this.reset();
		this.init = EMPTY;
		this.last = null;
		this.videoTrack = null;
	}

	/** `removeSourceBuffer`: what is in flight ends as aborted, and nothing runs after. */
	detach() {
		this.cancelDrain();
		if (this.op) this.end(this.op, "abort");
		this.removed = true;
		this.reset();
	}

	/**
	 * End of stream: flush the Opus encoder and append what it held, as the shim's own append (the
	 * page sees no event), then close it; a later append opens a new one. False when cancelled.
	 */
	drainAudio(): Promise<boolean> {
		// A second end of stream while the first drains is the same one.
		if (!this.audio) return Promise.resolve(true);
		if (this.drain && this.draining) return this.draining;
		const drain = new AbortController();
		this.drain = drain;
		const draining = this.drainFlush(drain);
		this.draining = draining;
		return draining;
	}

	private async drainFlush(drain: AbortController): Promise<boolean> {
		const audio = this.audio!;
		const live = () => !drain.signal.aborted && this.op === null && this.audio === audio;
		try {
			await audio.encoder.flush();
			if (!live()) return false;
			if (audio.error) throw audio.error;
			// The flush pads its last packet out to a whole Opus frame: one past the input is padding.
			const inputEnd = Math.round((audio.next * 1e6) / audio.timescale);
			const output = opusOutput(
				audio,
				audio.packets.splice(0).filter((packet) => packet.timestamp < inputEnd),
			);
			if (output) await this.realAppend(this.outputParts(null, output), drain.signal);
		} catch (e) {
			if (!live()) return false;
			// The stream still ends; only the held tail is lost.
			console.warn("[video] the audio held at the end of the stream was not appended:", e);
		} finally {
			if (this.drain === drain) {
				this.drain = null;
				this.draining = null;
			}
		}
		// Superseded: a contiguous page append may have taken this encoder over during our append.
		if (this.audio === audio && !drain.signal.aborted) {
			closeCodec(audio.encoder);
			this.audio = null;
		}
		return !drain.signal.aborted;
	}

	/**
	 * For a page call that cannot wait for the real SourceBuffer (it answers synchronously): the drain
	 * is cancelled and its append, if in flight, aborted. An append or a remove waits behind it instead.
	 */
	private supersede() {
		this.cancelDrain();
		this.abortOwn();
	}

	private cancelDrain() {
		this.drain?.abort();
		this.drain = null;
	}

	/** Aborts our append in flight. `abort()` also resets the append window, which is the page's: put back. */
	private abortOwn() {
		const own = this.own;
		if (!own || own.aborted) return;
		if (this.real.updating) {
			const { appendWindowStart, appendWindowEnd } = this.real;
			this.real.abort();
			this.real.appendWindowStart = appendWindowStart;
			this.real.appendWindowEnd = appendWindowEnd;
		}
		own.aborted = true;
	}

	private usable() {
		if (this.removed) throw new DOMException("This SourceBuffer has been removed from its MediaSource.", "InvalidStateError");
		if (this.updating) throw new DOMException("This SourceBuffer is still processing an operation.", "InvalidStateError");
	}

	private emit(type: string) {
		const event = new Event(type);
		this.dispatchEvent(event);
		(this as unknown as Record<string, ((event: Event) => void) | null>)[`on${type}`]?.call(this, event);
	}

	/** The real SourceBuffer's events: ours are the shim's to answer, any other is forwarded. */
	private onReal = (event: Event) => {
		const own = this.own;
		if (!own) return this.emit(event.type);
		if (event.type === "error" || event.type === "abort") own.failed = true;
		if (event.type === "updateend") {
			this.own = null;
			own.settle(own.failed);
		}
	};

	private async run(op: AbortController, bytes: Uint8Array) {
		// Asynchronous, as Chromium's: the caller sees `updating` before any event.
		await Promise.resolve();
		if (this.op !== op) return;
		this.emit("updatestart");
		const watchdog = setTimeout(() => {
			console.warn(`[video] a fragment did not transcode within ${watchdogMs} ms; cancelled`);
			try {
				this.abortOwn();
			} catch (e) {
				console.warn("[video] the real SourceBuffer did not abort:", e);
			}
			this.end(op, "error");
		}, watchdogMs);
		try {
			await this.transcode(bytes, op.signal);
			this.end(op, "update");
		} catch (e) {
			if (this.op === op) console.warn("[video] a fragment did not transcode:", e);
			this.end(op, "error");
		} finally {
			clearTimeout(watchdog);
		}
	}

	/**
	 * Ends `op` once with `outcome`, then `updateend`. A failed or abandoned fragment's encoder state
	 * is dropped: none of its output reaches the real SourceBuffer, and what it fed must not either.
	 */
	private end(op: AbortController, outcome: Outcome) {
		if (this.op !== op) return;
		this.op = null;
		op.abort();
		if (outcome === "update") {
			this.emit("update");
			this.emit("updateend");
			return;
		}
		this.reset();
		const fire = () => {
			this.emit(outcome);
			this.emit("updateend");
		};
		// `abort()` returns first, as Chromium's: the events follow, ahead of any next append's.
		if (outcome === "abort") queueMicrotask(fire);
		else fire();
	}

	private reset() {
		closeCodec(this.video?.encoder);
		closeCodec(this.audio?.encoder);
		this.video = null;
		this.audio = null;
		this.written = "";
	}

	private async transcode(bytes: Uint8Array, signal: AbortSignal) {
		const boxes = boxesIn(bytes);
		const before = this.init;
		this.init = initOf(bytes, boxes) ?? this.init;
		// An init alone touches nothing real: its VP9/Opus init goes out with the next fragment.
		if (!boxes.some((b) => b.type === "moof")) return;

		const spans = decodeSpans(bytes, trexDurations(this.init));
		// Rust primes its AAC decoder with the fragment before when the two touch, or a boundary
		// clicks; video gains nothing from it.
		const preroll = this.kinds.audio && this.last && contiguous(this.last.spans, spans) ? this.last.media : EMPTY;

		const frames: VideoFragment = { track: null, ticks: new Map(), end: 0 };
		if (this.video) this.video.chunks = [];
		for await (const record of decodeFragment(before, preroll, bytes, signal)) {
			signal.throwIfAborted();
			if (record.kind === "audio") this.encodeAudio(record);
			else await this.encodeVideo(record, frames, signal);
		}
		const video = await this.videoOutput(frames, signal);
		const audio = await this.audioOutput(signal);
		if (frames.track) this.videoTrack = frames.track;

		if (video || audio) await this.realAppend(this.outputParts(video, audio), signal);
		this.last = { media: bytes, spans };
	}

	/** One real append: the output init first when it changed, then a fragment per track. */
	private outputParts(video: Output | null, audio: Output | null) {
		const parts: Uint8Array[] = [];
		const init = this.outputInit();
		if (init.key !== this.written) {
			parts.push(writeInit(init.tracks));
			this.written = init.key;
		}
		if (video) parts.push(writeFragment(++this.seq, VIDEO_TRACK, video.base, video.samples));
		if (audio) parts.push(writeFragment(++this.seq, AUDIO_TRACK, audio.base, audio.samples));
		return concat(parts);
	}

	private async encodeVideo(record: VideoRecord, frames: VideoFragment, signal: AbortSignal) {
		const { width, height, timescale } = record;
		if (!frames.track) frames.track = { kind: "video", id: VIDEO_TRACK, timescale, width, height };
		else if (frames.track.width !== width || frames.track.height !== height || frames.track.timescale !== timescale)
			throw new Error("the fragment changes its frame size or timescale mid-way");
		// Before 0 (an edit list past the first frame): a frame wholly before it only served Rust as a
		// reference, one across it starts at 0. The first frame kept is the keyframe.
		if (record.pts + record.duration <= 0) return;
		const pts = Math.max(0, record.pts);

		let video = this.video;
		if (!video || video.width !== width || video.height !== height) {
			video ??= this.openVideo();
			video.encoder.configure(encoderConfig(width, height));
			video.width = width;
			video.height = height;
		}
		const timestamp = Math.round((pts * 1e6) / timescale);
		frames.ticks.set(timestamp, pts);
		frames.end = record.pts + record.duration;
		const chroma = (width / 2) * (height / 2);
		const frame = new VideoFrame(record.planes, {
			format: "I420",
			codedWidth: width,
			codedHeight: height,
			timestamp,
			layout: [
				{ offset: 0, stride: width },
				{ offset: width * height, stride: width / 2 },
				{ offset: width * height + chroma, stride: width / 2 },
			],
			colorSpace: BT709,
		});
		try {
			// Each fragment opens on a keyframe so it decodes on its own, wherever hls.js puts it.
			video.encoder.encode(frame, { keyFrame: frames.ticks.size === 1 });
		} finally {
			frame.close();
		}
		if (video.error) throw video.error;
		while (video.encoder.encodeQueueSize > MAX_QUEUED) {
			signal.throwIfAborted();
			await waitDequeue(video.encoder, signal);
		}
	}

	private openVideo(): VideoState {
		const encoder = new VideoEncoder({
			output: (chunk) => state.chunks.push(copyChunk(chunk)),
			error: (e) => (state.error = e),
		});
		const state: VideoState = { encoder, width: 0, height: 0, chunks: [] };
		this.video = state;
		return state;
	}

	/** The fragment's VP9, flushed: each frame keeps its input pts and lasts until the next one. */
	private async videoOutput(frames: VideoFragment, signal: AbortSignal): Promise<Output | null> {
		const video = this.video;
		if (!video || frames.ticks.size === 0) return null;
		await video.encoder.flush();
		signal.throwIfAborted();
		if (video.error) throw video.error;
		const timed = video.chunks.splice(0).map((chunk) => {
			const pts = frames.ticks.get(chunk.timestamp);
			if (pts === undefined) throw new Error(`the encoder returned a frame at ${chunk.timestamp} us, which was never sent`);
			return { chunk, pts };
		});
		const first = timed[0];
		if (!first) return null;
		const samples = timed.map(({ chunk, pts }, i) => ({ data: chunk.data, key: chunk.key, duration: (timed[i + 1]?.pts ?? frames.end) - pts }));
		return { base: first.pts, samples };
	}

	private encodeAudio(record: AudioRecord) {
		let audio = this.audio;
		// A seek, a remove and re-append, a gap: the encoder's state belongs to another stretch.
		if (!audio || !follows(audio, record)) {
			closeCodec(audio?.encoder);
			audio = this.openAudio(record);
		}
		const frames = record.pcm.length / record.channels;
		// An AAC priming edit list puts the first frame before 0, which no tfdt can say: cut it at 0,
		// as Chromium's append window would. The chain in `next` stays the raw one.
		const cut = record.pts < 0 ? Math.min(frames, Math.ceil((-record.pts * record.sampleRate) / record.timescale)) : 0;
		if (cut < frames) {
			const data = new AudioData({
				format: "f32",
				sampleRate: record.sampleRate,
				numberOfFrames: frames - cut,
				numberOfChannels: record.channels,
				timestamp: Math.max(0, Math.round((record.pts * 1e6) / record.timescale + (cut * 1e6) / record.sampleRate)),
				data: record.pcm.subarray(cut * record.channels),
			});
			try {
				audio.encoder.encode(data);
			} finally {
				data.close();
			}
		}
		if (audio.error) throw audio.error;
		audio.next = record.pts + record.duration;
	}

	private openAudio(record: AudioRecord): AudioState {
		const { sampleRate, channels, timescale } = record;
		const encoder = new AudioEncoder({
			output: (chunk, metadata) => {
				const description = metadata?.decoderConfig?.description;
				if (description && state.preSkip === undefined) state.preSkip = opusPreSkip(description);
				state.packets.push(copyChunk(chunk));
			},
			error: (e) => (state.error = e),
		});
		const state: AudioState = { encoder, sampleRate, channels, timescale, next: record.pts, packets: [], generation: ++this.generation };
		// At the source rate: `AudioData` must match it, and the encoder resamples to 48 kHz itself.
		encoder.configure({ codec: AUDIO_CODEC, sampleRate, numberOfChannels: channels, bitrate: OPUS_BITRATE });
		this.audio = state;
		return state;
	}

	/**
	 * The Opus packets out so far. The encoder is never flushed mid-stream: what it still holds
	 * goes out with the next fragment, contiguous with this one.
	 */
	private async audioOutput(signal: AbortSignal): Promise<Output | null> {
		const audio = this.audio;
		if (!audio) return null;
		while (audio.encoder.encodeQueueSize > 0) {
			signal.throwIfAborted();
			await waitDequeue(audio.encoder, signal);
		}
		// The outputs of what it dequeued come back as tasks of their own.
		await new Promise((resolve) => setTimeout(resolve, 0));
		signal.throwIfAborted();
		if (audio.error) throw audio.error;
		return opusOutput(audio, audio.packets.splice(0));
	}

	/** The output init for this SourceBuffer's tracks; its key changes with any of them. */
	private outputInit() {
		const tracks: TrackInit[] = [];
		if (this.kinds.video) {
			if (!this.videoTrack) throw new Error("a muxed fragment carries audio before any video frame");
			tracks.push(this.videoTrack);
		}
		if (this.kinds.audio) {
			const audio = this.audio;
			if (!audio || audio.end === undefined) throw new Error("a muxed fragment carries video before any Opus packet");
			tracks.push({ kind: "audio", id: AUDIO_TRACK, timescale: audio.timescale, channels: audio.channels, preSkip: audio.preSkip ?? 0 });
		}
		// A new Opus encoder primes again: its pre-skip needs an init of its own.
		return { tracks, key: `${JSON.stringify(tracks)}#${this.audio?.generation ?? 0}` };
	}

	private realAppend(bytes: Uint8Array<ArrayBuffer>, signal: AbortSignal) {
		return this.realRun(() => {
			this.real.timestampOffset = this.offset;
			this.real.appendBuffer(bytes);
		}, signal);
	}

	private async realRun(start: () => void, signal: AbortSignal) {
		// An append of ours that was abandoned still owes the real SourceBuffer's `updateend`.
		while (this.own) await this.own.done;
		signal.throwIfAborted();
		let settle!: (failed: boolean) => void;
		const done = new Promise<boolean>((resolve) => (settle = resolve));
		this.own = { failed: false, aborted: false, done, settle };
		try {
			start();
		} catch (e) {
			this.own = null;
			throw e;
		}
		if (await done) throw new Error("the real SourceBuffer refused a transcoded fragment");
	}
}

/** `window.MediaSource` from now on: Chromium's own, extended to take H.264 + AAC too. */
export function installTranscodingMediaSource(): void {
	const Real = RealMediaSource;
	if (!Real) {
		console.warn("[video] this page has no MediaSource to extend; H.264 clips stay refused");
		return;
	}
	const deadline = rustDeadlineMs("FRAGMENT_DEADLINE_MS");
	if (deadline === null) {
		console.warn("[video] the app published no fragment deadline; H.264 clips stay refused");
		return;
	}
	watchdogMs = deadline;
	class TranscodingMediaSource extends Real {
		private readonly shims = new Set<TranscodingSourceBuffer>();
		private ending: object | null = null;

		static override isTypeSupported(type: string): boolean {
			return shimIsTypeSupported(type);
		}

		override addSourceBuffer(type: string): SourceBuffer {
			if (!transcodable(type)) return super.addSourceBuffer(type);
			if (!shimIsTypeSupported(type)) throw new DOMException(`${type} cannot be transcoded until the video codec is ready`, "NotSupportedError");
			const kinds = kindsOf(type);
			const shim = new TranscodingSourceBuffer(super.addSourceBuffer(outputType(kinds)), kinds);
			this.shims.add(shim);
			return shim as unknown as SourceBuffer;
		}

		// `sourceBuffers` lists the real SourceBuffers; hls.js's detach removes a real one, and
		// either identity resolves to its shim, which must be detached whichever way it goes.
		override removeSourceBuffer(sourceBuffer: SourceBuffer): void {
			const shim = sourceBuffer instanceof TranscodingSourceBuffer ? sourceBuffer : [...this.shims].find((s) => s.real === sourceBuffer);
			if (!shim) return super.removeSourceBuffer(sourceBuffer);
			shim.detach();
			this.shims.delete(shim);
			super.removeSourceBuffer(shim.real);
		}

		/**
		 * The Opus encoders still hold the clip's last packets: they are drained into the
		 * real SourceBuffers first, and the real stream ends after. hls.js reads nothing in between.
		 */
		override endOfStream(error?: EndOfStreamError): void {
			const holding = [...this.shims].filter((shim) => shim.holdsAudio);
			if (error !== undefined || holding.length === 0) return super.endOfStream(error);
			if (this.readyState !== "open" || this.anyUpdating())
				throw new DOMException("The MediaSource is not open, or a SourceBuffer is still updating.", "InvalidStateError");
			const ending = {};
			this.ending = ending;
			void Promise.all(holding.map((shim) => shim.drainAudio())).then((drained) => {
				// A page append during the drain means the stream goes on: it ends again later.
				if (this.ending !== ending || !drained.every(Boolean) || this.readyState !== "open") return;
				this.ending = null;
				try {
					super.endOfStream();
				} catch (e) {
					console.warn("[video] the stream did not end after its audio drain:", e);
				}
			});
		}

		private anyUpdating() {
			if ([...this.shims].some((shim) => shim.updating)) return true;
			// A shim's real SourceBuffer is also busy with its own appends: its `updating` says what the page did.
			const reals = new Set([...this.shims].map((shim) => shim.real));
			for (let i = 0; i < this.sourceBuffers.length; i++) {
				const sb = this.sourceBuffers[i];
				if (sb && !reals.has(sb) && sb.updating) return true;
			}
			return false;
		}
	}
	(globalThis as any).MediaSource = TranscodingMediaSource;
}
