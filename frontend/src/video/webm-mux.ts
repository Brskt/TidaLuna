// Writes the WebM a transcoded cover is stored as: one VP9 track, one cluster per keyframe, with
// a seek head and cues so the media element finds its way back to the start of each loop.
//
// Written by hand rather than vendored: a cover is one short video track the page already holds
// whole, and Rust checks the result (`src/video/webm.rs`) before anything caches it.

export interface EncodedFrame {
	/** Presentation time in microseconds, the first at zero. */
	timestamp: number;
	key: boolean;
	data: Uint8Array;
}

export interface MuxInput {
	width: number;
	height: number;
	/** The last frame's time plus one frame, which `store/` checks against the decode. */
	durationMicros: number;
	frames: EncodedFrame[];
}

const ID = {
	ebml: 0x1a45dfa3,
	ebmlVersion: 0x4286,
	ebmlReadVersion: 0x42f7,
	ebmlMaxIdLength: 0x42f2,
	ebmlMaxSizeLength: 0x42f3,
	docType: 0x4282,
	docTypeVersion: 0x4287,
	docTypeReadVersion: 0x4285,
	segment: 0x18538067,
	seekHead: 0x114d9b74,
	seek: 0x4dbb,
	seekId: 0x53ab,
	seekPosition: 0x53ac,
	info: 0x1549a966,
	timecodeScale: 0x2ad7b1,
	duration: 0x4489,
	muxingApp: 0x4d80,
	writingApp: 0x5741,
	tracks: 0x1654ae6b,
	trackEntry: 0xae,
	trackNumber: 0xd7,
	trackUid: 0x73c5,
	trackType: 0x83,
	flagLacing: 0x9c,
	codecId: 0x86,
	video: 0xe0,
	pixelWidth: 0xb0,
	pixelHeight: 0xba,
	colour: 0x55b0,
	matrixCoefficients: 0x55b1,
	range: 0x55b9,
	transferCharacteristics: 0x55ba,
	primaries: 0x55bb,
	cluster: 0x1f43b675,
	timecode: 0xe7,
	simpleBlock: 0xa3,
	cues: 0x1c53bb6b,
	cuePoint: 0xbb,
	cueTime: 0xb3,
	cueTrackPositions: 0xb7,
	cueTrack: 0xf7,
	cueClusterPosition: 0xf1,
} as const;

/** Nanoseconds per tick (the default `TimecodeScale`): block timecodes count in milliseconds. */
const TIMECODE_SCALE = 1_000_000;

/** A block's timecode is a signed 16-bit offset from its cluster's. */
const MAX_BLOCK_OFFSET_MS = 32_767;

const concat = (parts: Uint8Array[]): Uint8Array<ArrayBuffer> => {
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const part of parts) {
		out.set(part, at);
		at += part.length;
	}
	return out;
};

const idBytes = (id: number): Uint8Array => {
	const bytes = [(id >>> 24) & 0xff, (id >>> 16) & 0xff, (id >>> 8) & 0xff, id & 0xff];
	while (bytes.length > 1 && bytes[0] === 0) bytes.shift();
	return Uint8Array.from(bytes);
};

/** Every size on eight bytes: wider than needed, and it keeps the seek head's length fixed. */
const sizeBytes = (size: number): Uint8Array => {
	const out = new Uint8Array(8);
	out[0] = 0x01;
	new DataView(out.buffer).setUint32(4, size);
	out[3] = Math.floor(size / 2 ** 32) & 0xff;
	return out;
};

const el = (id: number, ...body: Uint8Array[]): Uint8Array => {
	const data = concat(body);
	return concat([idBytes(id), sizeBytes(data.length), data]);
};

/** An unsigned integer on eight bytes, for the same reason as the sizes. */
const uint = (id: number, value: number): Uint8Array => {
	const out = new Uint8Array(8);
	new DataView(out.buffer).setBigUint64(0, BigInt(Math.round(value)));
	return el(id, out);
};

const float = (id: number, value: number): Uint8Array => {
	const out = new Uint8Array(8);
	new DataView(out.buffer).setFloat64(0, value);
	return el(id, out);
};

const text = (id: number, value: string): Uint8Array => el(id, new TextEncoder().encode(value));

const simpleBlock = (frame: EncodedFrame, offsetMs: number): Uint8Array => {
	const head = new Uint8Array(4);
	head[0] = 0x81; // track 1
	new DataView(head.buffer).setInt16(1, offsetMs);
	head[3] = frame.key ? 0x80 : 0x00;
	return el(ID.simpleBlock, head, frame.data);
};

/** One cluster per keyframe, and a new one before a block's offset would overflow. */
const clusterFrames = (frames: EncodedFrame[]): { timeMs: number; frames: EncodedFrame[] }[] => {
	const clusters: { timeMs: number; frames: EncodedFrame[] }[] = [];
	for (const frame of frames) {
		const timeMs = Math.round(frame.timestamp / 1000);
		const current = clusters[clusters.length - 1];
		if (!current || frame.key || timeMs - current.timeMs > MAX_BLOCK_OFFSET_MS) {
			clusters.push({ timeMs, frames: [frame] });
		} else {
			current.frames.push(frame);
		}
	}
	return clusters;
};

export function muxWebm(input: MuxInput): Uint8Array<ArrayBuffer> {
	const header = el(
		ID.ebml,
		uint(ID.ebmlVersion, 1),
		uint(ID.ebmlReadVersion, 1),
		uint(ID.ebmlMaxIdLength, 4),
		uint(ID.ebmlMaxSizeLength, 8),
		text(ID.docType, "webm"),
		uint(ID.docTypeVersion, 4),
		uint(ID.docTypeReadVersion, 2),
	);

	const info = el(
		ID.info,
		uint(ID.timecodeScale, TIMECODE_SCALE),
		float(ID.duration, input.durationMicros / 1000),
		text(ID.muxingApp, "TidaLunar"),
		text(ID.writingApp, "TidaLunar"),
	);

	// BT.709, limited range, as `BT709` in cover-encode.ts tags the frames.
	const tracks = el(
		ID.tracks,
		el(
			ID.trackEntry,
			uint(ID.trackNumber, 1),
			uint(ID.trackUid, 1),
			uint(ID.trackType, 1),
			uint(ID.flagLacing, 0),
			text(ID.codecId, "V_VP9"),
			el(
				ID.video,
				uint(ID.pixelWidth, input.width),
				uint(ID.pixelHeight, input.height),
				el(
					ID.colour,
					uint(ID.matrixCoefficients, 1),
					uint(ID.range, 1),
					uint(ID.transferCharacteristics, 1),
					uint(ID.primaries, 1),
				),
			),
		),
	);

	const clusters = clusterFrames(input.frames).map((group) => ({
		timeMs: group.timeMs,
		bytes: el(
			ID.cluster,
			uint(ID.timecode, group.timeMs),
			...group.frames.map((frame) =>
				simpleBlock(frame, Math.round(frame.timestamp / 1000) - group.timeMs),
			),
		),
	}));

	// Positions count from the segment's data. The seek head's own length does not depend on the
	// positions it holds (fixed-width integers); it is measured with zeros first.
	const seekHead = (at: { info: number; tracks: number; cues: number }) =>
		el(
			ID.seekHead,
			...(
				[
					[ID.info, at.info],
					[ID.tracks, at.tracks],
					[ID.cues, at.cues],
				] as const
			).map(([id, position]) => el(ID.seek, el(ID.seekId, idBytes(id)), uint(ID.seekPosition, position))),
		);
	const infoAt = seekHead({ info: 0, tracks: 0, cues: 0 }).length;
	const tracksAt = infoAt + info.length;
	let at = tracksAt + tracks.length;
	const cuePoints = clusters.map((cluster) => {
		const point = el(
			ID.cuePoint,
			uint(ID.cueTime, cluster.timeMs),
			el(ID.cueTrackPositions, uint(ID.cueTrack, 1), uint(ID.cueClusterPosition, at)),
		);
		at += cluster.bytes.length;
		return point;
	});
	const cuesAt = at;
	const cues = el(ID.cues, ...cuePoints);

	const segment = el(
		ID.segment,
		seekHead({ info: infoAt, tracks: tracksAt, cues: cuesAt }),
		info,
		tracks,
		...clusters.map((cluster) => cluster.bytes),
		cues,
	);
	return concat([header, segment]);
}
