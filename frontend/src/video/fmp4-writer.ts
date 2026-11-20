// Fragmented MP4 for the transcoding MediaSource: an init per track set, then one `moof` + `mdat`
// per track and append. Hand-written: the fragment must carry its own base time, in the input
// track's timescale, to land where hls.js timed the input.

export type TrackInit =
	| { kind: "video"; id: number; timescale: number; width: number; height: number }
	| { kind: "audio"; id: number; timescale: number; channels: number; preSkip: number };

export type FragmentSample = { data: Uint8Array; duration: number; key: boolean };

/** Bytes written big-endian, as every ISO BMFF field is. */
class Fields {
	private parts: number[] = [];

	u8(v: number) {
		this.parts.push(v & 0xff);
		return this;
	}
	u16(v: number) {
		return this.u8(v >>> 8).u8(v);
	}
	u32(v: number) {
		return this.u16(Math.floor(v / 0x10000)).u16(v);
	}
	u64(v: number) {
		return this.u32(Math.floor(v / 0x100000000)).u32(v >>> 0);
	}
	zeros(n: number) {
		for (let i = 0; i < n; i++) this.u8(0);
		return this;
	}
	text(s: string) {
		for (const c of s) this.u8(c.charCodeAt(0));
		return this;
	}
	bytes() {
		return Uint8Array.from(this.parts);
	}
}

const concat = (parts: Uint8Array[]) => {
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
};

const box = (type: string, ...body: Uint8Array[]) => {
	const content = concat(body);
	return concat([new Fields().u32(8 + content.length).text(type).bytes(), content]);
};

const fullBox = (type: string, version: number, flags: number, ...body: Uint8Array[]) =>
	box(type, new Fields().u8(version).u8(flags >>> 16).u16(flags).bytes(), ...body);

const fields = () => new Fields();

const UNITY_MATRIX = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];

const matrix = (f: Fields) => {
	for (const v of UNITY_MATRIX) f.u32(v);
	return f;
};

/** Opus decodes at 48 kHz whatever rate went in; `dOps` and the entry say so. */
const OPUS_RATE = 48000;

/** A VisualSampleEntry's fields, data reference 1, 72 dpi, one frame per sample, 24-bit depth. */
const visualFields = (width: number, height: number) =>
	fields()
		.zeros(6)
		.u16(1)
		.zeros(16)
		.u16(width)
		.u16(height)
		.u32(0x00480000)
		.u32(0x00480000)
		.u32(0)
		.u16(1)
		.zeros(32)
		.u16(0x0018)
		.u16(0xffff)
		.bytes();

/** VP9 profile 0, level 4.0, 8-bit 4:2:0, BT.709 limited range: what `vp09.00.40.08` names. */
const vp09Entry = (width: number, height: number) =>
	box(
		"vp09",
		visualFields(width, height),
		fullBox(
			"vpcC",
			1,
			0,
			fields()
				.u8(0)
				.u8(40)
				// bit depth 8, chroma 4:2:0 co-located (1), limited range.
				.u8((8 << 4) | (1 << 1))
				.u8(1)
				.u8(1)
				.u8(1)
				.u16(0)
				.bytes(),
		),
	);

const opusEntry = (channels: number, preSkip: number) =>
	box(
		"Opus",
		fields()
			.zeros(6)
			.u16(1)
			.zeros(8)
			.u16(channels)
			.u16(16)
			.u32(0)
			.u32(OPUS_RATE * 0x10000)
			.bytes(),
		box("dOps", fields().u8(0).u8(channels).u16(preSkip).u32(OPUS_RATE).u16(0).u8(0).bytes()),
	);

const trak = (track: TrackInit) => {
	const video = track.kind === "video";
	const tkhd = fullBox(
		"tkhd",
		0,
		0x3,
		matrix(
			fields()
				.u32(0)
				.u32(0)
				.u32(track.id)
				.u32(0)
				.u32(0)
				.zeros(8)
				.u16(0)
				.u16(0)
				.u16(video ? 0 : 0x0100)
				.u16(0),
		)
			.u32(video ? track.width * 0x10000 : 0)
			.u32(video ? track.height * 0x10000 : 0)
			.bytes(),
	);
	// Language `und`, packed as three 5-bit letters.
	const mdhd = fullBox("mdhd", 0, 0, fields().u32(0).u32(0).u32(track.timescale).u32(0).u16(0x55c4).u16(0).bytes());
	const hdlr = fullBox(
		"hdlr",
		0,
		0,
		fields()
			.u32(0)
			.text(video ? "vide" : "soun")
			.zeros(12)
			.text(video ? "VideoHandler" : "SoundHandler")
			.u8(0)
			.bytes(),
	);
	const header = video ? fullBox("vmhd", 0, 1, fields().zeros(8).bytes()) : fullBox("smhd", 0, 0, fields().zeros(4).bytes());
	const dinf = box("dinf", fullBox("dref", 0, 0, fields().u32(1).bytes(), fullBox("url ", 0, 1)));
	const entry = video ? vp09Entry(track.width, track.height) : opusEntry(track.channels, track.preSkip);
	const empty = fields().u32(0).bytes();
	const stbl = box(
		"stbl",
		fullBox("stsd", 0, 0, fields().u32(1).bytes(), entry),
		fullBox("stts", 0, 0, empty),
		fullBox("stsc", 0, 0, empty),
		fullBox("stsz", 0, 0, fields().u32(0).u32(0).bytes()),
		fullBox("stco", 0, 0, empty),
	);
	return box("trak", tkhd, box("mdia", mdhd, hdlr, box("minf", header, dinf, stbl)));
};

/** `ftyp` + `moov` for `tracks`, each timed in its own timescale, samples described by `trex`. */
export function writeInit(tracks: TrackInit[]): Uint8Array<ArrayBuffer> {
	const ftyp = box("ftyp", fields().text("isom").u32(0x200).text("isom").text("iso6").text("mp41").bytes());
	const nextId = Math.max(0, ...tracks.map((t) => t.id)) + 1;
	const mvhd = fullBox(
		"mvhd",
		0,
		0,
		matrix(fields().u32(0).u32(0).u32(1000).u32(0).u32(0x00010000).u16(0x0100).zeros(10))
			.zeros(24)
			.u32(nextId)
			.bytes(),
	);
	const trex = tracks.map((t) => fullBox("trex", 0, 0, fields().u32(t.id).u32(1).u32(0).u32(0).u32(0).bytes()));
	return concat([ftyp, box("moov", mvhd, ...tracks.map(trak), box("mvex", ...trex))]);
}

/** A sync sample; any other depends on others and is not one. */
const KEY_FLAGS = 0x02000000;
const DELTA_FLAGS = 0x01010000;

/** trun fields present: data offset, then per sample its duration, size and flags. */
const TRUN_FLAGS = 0x000001 | 0x000100 | 0x000200 | 0x000400;

/** tfhd: data offsets count from the `moof`'s first byte. */
const DEFAULT_BASE_IS_MOOF = 0x020000;

/**
 * One `moof` + `mdat` for `trackId`, its first sample decoded at `baseTime` in the track's
 * timescale. The `trun`'s data offset points at the first sample inside the `mdat`.
 */
export function writeFragment(seq: number, trackId: number, baseTime: number, samples: FragmentSample[]): Uint8Array<ArrayBuffer> {
	if (!Number.isSafeInteger(baseTime) || baseTime < 0) throw new RangeError(`a fragment cannot start at ${baseTime}`);
	const build = (dataOffset: number) => {
		const run = fields().u32(samples.length).u32(dataOffset);
		for (const s of samples) run.u32(s.duration).u32(s.data.length).u32(s.key ? KEY_FLAGS : DELTA_FLAGS);
		return box(
			"moof",
			fullBox("mfhd", 0, 0, fields().u32(seq).bytes()),
			box(
				"traf",
				fullBox("tfhd", 0, DEFAULT_BASE_IS_MOOF, fields().u32(trackId).bytes()),
				fullBox("tfdt", 1, 0, fields().u64(baseTime).bytes()),
				fullBox("trun", 0, TRUN_FLAGS, run.bytes()),
			),
		);
	};
	// The offset's own value does not change the `moof`'s size: measure, then write it.
	const moofSize = build(0).length;
	return concat([build(moofSize + 8), box("mdat", ...samples.map((s) => s.data))]);
}
