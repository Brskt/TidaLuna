// Transcodes one animated cover to VP9, once: Rust decodes TIDAL's H.264 (`luna-cover://frames/`),
// the page encodes with WebCodecs and posts the WebM back (`luna-cover://store/`), and from then on
// TIDAL's own `<video>` plays it from the cache (`luna-cover://video/`).
//
// This build of Chromium decodes VP9 and not H.264. Encoding here rather than in Rust uses the
// libvpx Chromium already carries; Rust has no maintained VP9 encoder binding.

import { muxWebm, type EncodedFrame } from "./webm-mux";

/**
 * What one attempt came to. Only `failed` blames the cover; the rest is the cache or the machine.
 * - `stored`: the cover is cached (by this pass, or another window's), a reload plays it.
 * - `retry-soon`: nothing was done yet (Rust busy, the artwork host unreachable); cheap to ask again.
 * - `retry-on-miss`: the machine failed a whole pass (deadline, encoder, full disk); asked again
 *   only when an element misses, a pass being costly.
 * - `unavailable`: the codec is off, missing or not loaded; nothing to do until the next miss.
 * - `no-encoder`: this Chromium cannot encode VP9, which holds for the whole session.
 * - `failed`: the cover does not transcode, or the cause is unknown; an attempt spent.
 */
export type TranscodeOutcome = "stored" | "retry-soon" | "retry-on-miss" | "unavailable" | "no-encoder" | "failed";

/** `frames/` refused: Rust's status names the stage that failed. */
const framesOutcome = (status: number): TranscodeOutcome =>
	status === 429 || status === 502 ? "retry-soon" : status === 503 ? "unavailable" : "failed";

/** `store/` refused. 409: no pass awaits this upload, mostly another window's store; the reload tells. */
const storeOutcome = (status: number): TranscodeOutcome =>
	status === 409 ? "stored" : status === 507 ? "retry-on-miss" : "failed";

/** A thrown cause that names the machine heals with time; any other, or none, spends. */
const thrownOutcome = (cause: unknown, signal?: AbortSignal): TranscodeOutcome =>
	signal?.aborted || (cause instanceof DOMException && cause.name === "EncodingError") ? "retry-on-miss" : "failed";

const MAGIC = "LVC2";
const HEADER_LEN = 16;
const RECORD_LEN = 32;

/**
 * VP9 profile 0, level 4.0, 8-bit, in software (no hardware encoder was probed; it runs once per
 * cover), variable bitrate at 8 Mbps: 4 Mbps still showed artifacts next to TIDAL's own MP4.
 * Any change here or in the muxer bumps `COVER_FORMAT` in `src/video/cover_cache.rs`.
 */
export const encoderConfig = (width: number, height: number): VideoEncoderConfig => ({
	codec: "vp09.00.40.08",
	width,
	height,
	bitrate: 8_000_000,
	bitrateMode: "variable",
	hardwareAcceleration: "prefer-software",
	latencyMode: "quality",
});

/** Every 90 frames: three seconds of a 30 fps loop to decode at most after a seek. */
const KEYFRAME_INTERVAL = 90;

/** Frames queued in the encoder before the reader waits for it. */
const MAX_QUEUED = 8;

/**
 * Named, never left to the default: WebCodecs would tag the frames BT.709 anyway, but the decoded
 * planes carry no tag and an untagged VP9 is free to be rendered BT.601.
 * Chromium renders TIDAL's untagged H.264 as BT.709, which this matches.
 */
export const BT709: VideoColorSpaceInit = {
	primaries: "bt709",
	transfer: "bt709",
	matrix: "bt709",
	fullRange: false,
};

/** Reads exact byte counts off a stream body, carrying the remainder of each chunk over. */
class ByteReader {
	private chunks: Uint8Array[] = [];
	private held = 0;

	constructor(private reader: ReadableStreamDefaultReader<Uint8Array>) {}

	/** `n` bytes into `into` (from its start), or false if the body ends first. */
	async readInto(into: Uint8Array, n: number): Promise<boolean> {
		while (this.held < n) {
			const { done, value } = await this.reader.read();
			if (done) return false;
			this.chunks.push(value);
			this.held += value.length;
		}
		let at = 0;
		let whole = 0;
		for (const chunk of this.chunks) {
			if (at === n) break;
			const take = Math.min(chunk.length, n - at);
			into.set(chunk.subarray(0, take), at);
			at += take;
			if (take < chunk.length) {
				this.chunks[whole] = chunk.subarray(take);
				break;
			}
			whole++;
		}
		this.chunks.splice(0, whole);
		this.held -= n;
		return true;
	}

	cancel() {
		void this.reader.cancel().catch(() => {});
	}
}

const coverQuery = (real: string, edge: number) => `?src=${encodeURIComponent(real)}&edge=${edge}`;

const reasonOf = (res: Response) => res.headers.get("X-Luna-Cover-Reason") ?? `status ${res.status}`;

/** Whether this Chromium has the encoder at all; asked once, the answer does not change. */
let encoderSupport: Promise<boolean> | undefined;
const encoderSupported = (config: VideoEncoderConfig) => {
	encoderSupport ??=
		typeof VideoEncoder === "undefined"
			? Promise.resolve(false)
			: VideoEncoder.isConfigSupported(config).then(
					(r) => !!r.supported,
					() => false,
				);
	return encoderSupport;
};

/**
 * Transcode the cover `real` at `edge` and hand it to Rust's cache. `signal` abandons it: the
 * frames body is cancelled and the encoder closed, which ends every wait below.
 */
export async function transcodeCover(real: string, edge: number, signal?: AbortSignal): Promise<TranscodeOutcome> {
	const query = coverQuery(real, edge);
	let res: Response;
	try {
		res = await fetch(`luna-cover://frames/${query}`, { signal });
	} catch (e) {
		// Rust answers every refusal with a status: a request that throws is not the cover.
		console.warn("[cover] frames request failed:", e);
		return "retry-on-miss";
	}
	if (!res.ok || !res.body) {
		console.log("[cover] not transcoded:", reasonOf(res));
		return framesOutcome(res.status);
	}

	const body = new ByteReader(res.body.getReader());
	let encoder: VideoEncoder | undefined;
	let encodeError: unknown;
	const abandon = () => {
		body.cancel();
		if (encoder && encoder.state !== "closed") encoder.close();
	};
	signal?.addEventListener("abort", abandon, { once: true });
	try {
		const head = new Uint8Array(HEADER_LEN);
		if (!(await body.readInto(head, HEADER_LEN))) throw new Error("the frames body ends in its header");
		const view = new DataView(head.buffer);
		const magic = String.fromCharCode(...head.subarray(0, 4));
		if (magic !== MAGIC) throw new Error(`the frames body is not ${MAGIC}`);
		const width = view.getUint32(4, true);
		const height = view.getUint32(8, true);
		const count = view.getUint32(12, true);

		const config = encoderConfig(width, height);
		if (!(await encoderSupported(config))) {
			console.log("[cover] this build has no VP9 encoder; covers stay still");
			body.cancel();
			return "no-encoder";
		}

		const frames: EncodedFrame[] = [];
		const enc = new VideoEncoder({
			output: (chunk) => {
				const data = new Uint8Array(chunk.byteLength);
				chunk.copyTo(data);
				frames.push({ timestamp: chunk.timestamp, key: chunk.type === "key", data });
			},
			error: (e) => {
				encodeError = e;
			},
		});
		encoder = enc;
		enc.configure(config);

		const record = new Uint8Array(RECORD_LEN);
		const recordView = new DataView(record.buffer);
		let planes = new Uint8Array(0);
		let lastPts = 0;
		for (let i = 0; i < count; i++) {
			if (!(await body.readInto(record, RECORD_LEN))) throw new Error(`the frames body ends at frame ${i}`);
			const pts = Number(recordView.getBigInt64(0, true));
			const field = (at: number) => recordView.getUint32(at, true);
			const [yStride, uStride, vStride] = [field(8), field(12), field(16)];
			const [yLen, uLen, vLen] = [field(20), field(24), field(28)];
			const total = yLen + uLen + vLen;
			// One buffer for every frame: `VideoFrame` copies what it is given.
			if (planes.length < total) planes = new Uint8Array(total);
			if (!(await body.readInto(planes, total))) throw new Error(`the frames body ends inside frame ${i}`);

			const frame = new VideoFrame(planes.subarray(0, total), {
				format: "I420",
				codedWidth: width,
				codedHeight: height,
				timestamp: pts,
				layout: [
					{ offset: 0, stride: yStride },
					{ offset: yLen, stride: uStride },
					{ offset: yLen + uLen, stride: vStride },
				],
				colorSpace: BT709,
			});
			try {
				enc.encode(frame, { keyFrame: i % KEYFRAME_INTERVAL === 0 });
			} finally {
				frame.close();
			}
			lastPts = pts;
			if (encodeError) throw encodeError;
			while (enc.encodeQueueSize > MAX_QUEUED) {
				if (signal?.aborted) throw new Error("the transcode was abandoned");
				await new Promise((resolve) => {
					enc.addEventListener("dequeue", resolve, { once: true });
					signal?.addEventListener("abort", resolve, { once: true });
				});
			}
		}
		// Read to the end before the upload: Rust opens the upload's grant at the end of the body,
		// and a body longer than its header said is not the cover it claims.
		if (await body.readInto(new Uint8Array(1), 1)) throw new Error("the frames body runs past its last frame");
		await enc.flush();
		if (encodeError) throw encodeError;
		enc.close();
		encoder = undefined;
		if (frames.length !== count) throw new Error(`encoded ${frames.length} of ${count} frames`);

		// The same rule as Rust's grant: the last frame's time plus one frame.
		const frameMicros = count > 1 ? Math.trunc(lastPts / (count - 1)) : 0;
		const webm = muxWebm({ width, height, durationMicros: lastPts + frameMicros, frames });

		// A typed array, never a `Blob`: CEF hands a scheme handler a `Blob` body as nothing. And no
		// headers: one would make the request preflighted, which the scheme does not answer.
		let stored: Response;
		try {
			stored = await fetch(`luna-cover://store/${query}`, { method: "POST", body: webm, signal });
		} catch (e) {
			// As for `frames/`: a request that throws is not the cover.
			console.warn("[cover] store request failed:", e);
			return "retry-on-miss";
		}
		if (!stored.ok) {
			console.warn("[cover] store refused:", reasonOf(stored));
			return storeOutcome(stored.status);
		}
		return "stored";
	} catch (e) {
		console.warn("[cover] transcode failed:", e);
		body.cancel();
		// An encode on a closed encoder throws its own error: the callback's says why it closed.
		return thrownOutcome(encodeError ?? e, signal);
	} finally {
		signal?.removeEventListener("abort", abandon);
		if (encoder && encoder.state !== "closed") encoder.close();
	}
}
