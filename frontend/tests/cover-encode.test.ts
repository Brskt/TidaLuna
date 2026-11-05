// Tests `src/video/cover-encode.ts` against a fake `luna-cover://` and a fake WebCodecs (happy-dom
// has neither): what it reads off the frames body, how it tags the frames, and the shape of the
// upload, which CEF and CORS both drop in silence when it is wrong.

import { expect, test } from "bun:test";

import { BT709, transcodeCover } from "../src/video/cover-encode";

const REAL = "https://resources.tidal.com/videos/a/b/1280x1280.mp4";

/** An LVC2 body: 2x2 frames, Y 2x2 at stride 4, U and V 1x1 at stride 2. */
const framesBody = (pts: number[]) => {
	const planes = [8, 2, 2];
	const parts: Uint8Array[] = [];
	const head = new DataView(new ArrayBuffer(16));
	"LVC2".split("").forEach((c, i) => head.setUint8(i, c.charCodeAt(0)));
	head.setUint32(4, 2, true);
	head.setUint32(8, 2, true);
	head.setUint32(12, pts.length, true);
	parts.push(new Uint8Array(head.buffer));
	for (const t of pts) {
		const record = new DataView(new ArrayBuffer(32));
		record.setBigInt64(0, BigInt(t), true);
		[4, 2, 2, ...planes].forEach((v, i) => record.setUint32(8 + i * 4, v, true));
		parts.push(new Uint8Array(record.buffer), new Uint8Array(planes.reduce((a, b) => a + b)));
	}
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
};

const fakeWebCodecs = ({ encoderError }: { encoderError?: DOMException } = {}) => {
	const frameInits: VideoFrameBufferInit[] = [];
	(globalThis as any).VideoFrame = class {
		timestamp: number;
		constructor(_data: BufferSource, init: VideoFrameBufferInit) {
			frameInits.push(init);
			this.timestamp = init.timestamp;
		}
		close() {}
	};
	(globalThis as any).VideoEncoder = class {
		static isConfigSupported = async () => ({ supported: true });
		encodeQueueSize = 0;
		state = "unconfigured";
		constructor(private init: VideoEncoderInit) {}
		configure() {
			this.state = "configured";
		}
		encode(frame: { timestamp: number }, opts?: VideoEncoderEncodeOptions) {
			if (this.state === "closed") throw new DOMException("the encoder is closed", "InvalidStateError");
			if (encoderError) {
				// As WebCodecs does: the error comes later, and the next encode meets a closed encoder.
				queueMicrotask(() => {
					this.state = "closed";
					this.init.error(encoderError);
				});
				return;
			}
			this.init.output(
				{
					timestamp: frame.timestamp,
					type: opts?.keyFrame ? "key" : "delta",
					byteLength: 1,
					copyTo: (into: Uint8Array) => (into[0] = 7),
				} as unknown as EncodedVideoChunk,
				{},
			);
		}
		async flush() {}
		close() {
			this.state = "closed";
		}
		addEventListener() {}
	};
	return frameInits;
};

test("the frames are encoded tagged BT.709 and the WebM is posted as bytes with no header", async () => {
	const frameInits = fakeWebCodecs();
	const posts: { url: string; init?: RequestInit }[] = [];
	globalThis.fetch = (async (url: string, init?: RequestInit) => {
		if (url.startsWith("luna-cover://frames/")) return new Response(framesBody([0, 33_333, 66_666]));
		posts.push({ url, init });
		return new Response(null, { status: 200 });
	}) as typeof fetch;

	expect(await transcodeCover(REAL, 1280)).toBe("stored");

	expect(frameInits.map((init) => init.colorSpace)).toEqual([BT709, BT709, BT709]);
	// `toEqual` fails on any property it is not given: a header set would show here.
	expect(posts).toEqual([
		{
			url: `luna-cover://store/?src=${encodeURIComponent(REAL)}&edge=1280`,
			init: { method: "POST", body: expect.any(Uint8Array) },
		},
	]);
});

/**
 * `frames/` answers `frames` (and `body`), `store/` answers `store`; a status of 0 throws as a
 * dropped request. Returns the store requests made.
 */
const serve = (frames: number, store = 200, body = framesBody([0, 33_333])) => {
	const posted: string[] = [];
	globalThis.fetch = (async (url: string, init?: RequestInit) => {
		if (init?.signal?.aborted) throw new DOMException("aborted", "AbortError");
		const isFrames = url.startsWith("luna-cover://frames/");
		const status = isFrames ? frames : store;
		if (status === 0) throw new TypeError("Failed to fetch");
		if (!isFrames) posted.push(url);
		if (isFrames && status === 200) return new Response(body);
		return new Response(null, { status });
	}) as typeof fetch;
	return posted;
};

// Rust opens the upload's grant at the end of the body: the page reads to it before posting.
test("a frames body that runs past its last frame is refused before anything is posted", async () => {
	fakeWebCodecs();
	const body = framesBody([0, 33_333]);
	const longer = new Uint8Array(body.length + 1);
	longer.set(body);
	const posted = serve(200, 200, longer);

	expect(await transcodeCover(REAL, 1280)).toBe("failed");
	expect(posted).toEqual([]);
});

// Only `failed` spends one of a cover's two attempts: a machine fault read as the cover's freezes
// a playable cover for the session, and the reverse transcodes a broken one without end.
test("each refusal comes back as what the page does about it", async () => {
	fakeWebCodecs();
	for (const [frames, store, outcome] of [
		[429, 200, "retry-soon"],
		[502, 200, "retry-soon"],
		[503, 200, "unavailable"],
		[422, 200, "failed"],
		[0, 200, "retry-on-miss"],
		[200, 409, "stored"],
		[200, 507, "retry-on-miss"],
		[200, 0, "retry-on-miss"],
		[200, 422, "failed"],
	] as const) {
		serve(frames, store);
		expect([frames, store, await transcodeCover(REAL, 1280)]).toEqual([frames, store, outcome]);
	}
});

test("an encoder that failed or a deadline asks again, an encoder that refuses the cover spends", async () => {
	serve(200);
	fakeWebCodecs({ encoderError: new DOMException("the encoder failed", "EncodingError") });
	expect(await transcodeCover(REAL, 1280)).toBe("retry-on-miss");
	fakeWebCodecs({ encoderError: new DOMException("unsupported", "NotSupportedError") });
	expect(await transcodeCover(REAL, 1280)).toBe("failed");

	// The deadline lands while the encoder is full: what then throws is the abandon, not the cover.
	fakeWebCodecs();
	const deadline = new AbortController();
	const Encoder = (globalThis as any).VideoEncoder;
	(globalThis as any).VideoEncoder = class extends Encoder {
		encodeQueueSize = 100;
		encode(frame: { timestamp: number }, opts?: VideoEncoderEncodeOptions) {
			super.encode(frame, opts);
			deadline.abort();
		}
	};
	expect(await transcodeCover(REAL, 1280, deadline.signal)).toBe("retry-on-miss");
});
