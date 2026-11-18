// One H.264 + AAC fMP4 append decoded by Rust (`luna-cover://fragment/`): the I420 frames and f32
// PCM come back as `LVF1` records, every audio record first.

import { ByteReader } from "./cover-encode";

export type VideoRecord = { kind: "video"; timescale: number; pts: number; duration: number; width: number; height: number; planes: Uint8Array };
export type AudioRecord = {
	kind: "audio";
	timescale: number;
	pts: number;
	duration: number;
	sampleRate: number;
	channels: number;
	pcm: Float32Array<ArrayBuffer>;
};

const MAGIC = "LVF1";
const VIDEO_TAG = 0;
const AUDIO_TAG = 1;

/** Past the tag: timescale, pts, duration, width, height. */
const VIDEO_HEAD = 4 + 8 + 4 + 4 + 4;
/** Past the tag: timescale, pts, duration, sample rate, channels, frames. */
const AUDIO_HEAD = 4 + 8 + 4 + 4 + 4 + 4;

/** `u32 BE init_len | init | u32 BE preroll_len | preroll | media`, as Rust's `parse_body` reads it. */
const requestBody = (init: Uint8Array, preroll: Uint8Array, media: Uint8Array) => {
	const body = new Uint8Array(8 + init.length + preroll.length + media.length);
	const view = new DataView(body.buffer);
	view.setUint32(0, init.length);
	body.set(init, 4);
	view.setUint32(4 + init.length, preroll.length);
	body.set(preroll, 8 + init.length);
	body.set(media, 8 + init.length + preroll.length);
	return body;
};

/**
 * Decode `media` with the SourceBuffer's last `init`, and the append before as `preroll` when the
 * two touch. Throws on a refusal or a broken body, possibly after records were yielded: a caller
 * keeps nothing of a fragment that throws.
 */
export async function* decodeFragment(
	init: Uint8Array,
	preroll: Uint8Array,
	media: Uint8Array,
	signal: AbortSignal,
): AsyncGenerator<VideoRecord | AudioRecord> {
	// A typed array, never a `Blob`: CEF hands a scheme handler a `Blob` body as nothing. And no
	// headers: one would make the request preflighted, which the scheme does not answer.
	const res = await fetch("luna-cover://fragment/", { method: "POST", body: requestBody(init, preroll, media), signal });
	if (!res.ok || !res.body) {
		const reason = res.headers.get("X-Luna-Cover-Reason") ?? "";
		throw new Error(`the fragment was refused: ${res.status} ${reason}`.trim());
	}
	const body = new ByteReader(res.body.getReader());
	signal.addEventListener("abort", () => body.cancel(), { once: true });
	try {
		const magic = new Uint8Array(4);
		if (!(await body.readInto(magic, 4)) || String.fromCharCode(...magic) !== MAGIC) throw new Error(`the fragment body is not ${MAGIC}`);
		const tag = new Uint8Array(1);
		const head = new Uint8Array(AUDIO_HEAD);
		const view = new DataView(head.buffer);
		const u32 = (at: number) => view.getUint32(at, true);
		while (await body.readInto(tag, 1)) {
			if (tag[0] === VIDEO_TAG) {
				if (!(await body.readInto(head, VIDEO_HEAD))) throw new Error("the fragment body ends in a frame's header");
				const [width, height] = [u32(16), u32(20)];
				const planes = new Uint8Array(width * height + 2 * (width / 2) * (height / 2));
				if (!(await body.readInto(planes, planes.length))) throw new Error("the fragment body ends inside a frame");
				yield { kind: "video", timescale: u32(0), pts: Number(view.getBigInt64(4, true)), duration: u32(12), width, height, planes };
			} else if (tag[0] === AUDIO_TAG) {
				if (!(await body.readInto(head, AUDIO_HEAD))) throw new Error("the fragment body ends in a PCM header");
				const [sampleRate, channels, frames] = [u32(16), u32(20), u32(24)];
				const bytes = new Uint8Array(frames * channels * 4);
				if (!(await body.readInto(bytes, bytes.length))) throw new Error("the fragment body ends inside its PCM");
				const pcm = new Float32Array(bytes.buffer);
				// Records are little-endian, as is every platform this app ships on.
				yield { kind: "audio", timescale: u32(0), pts: Number(view.getBigInt64(4, true)), duration: u32(12), sampleRate, channels, pcm };
			} else {
				throw new Error(`the fragment body holds an unknown record ${tag[0]}`);
			}
		}
	} finally {
		body.cancel();
	}
}
