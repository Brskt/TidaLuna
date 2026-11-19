// The input side of `fmp4-writer.ts`: the boxes of an fMP4 append hls.js makes, the init it
// carries and each track's span of decode time.

export type Box = { type: string; start: number; body: number; end: number };

/** The boxes laid end to end in `data[from, to)`, refusing one that runs past `to`. */
export const boxesIn = (data: Uint8Array, from = 0, to = data.length): Box[] => {
	const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
	const out: Box[] = [];
	for (let at = from; at < to; ) {
		if (to - at < 8) throw new Error("a box header is cut short");
		const type = String.fromCharCode(...data.subarray(at + 4, at + 8));
		let size = view.getUint32(at);
		let body = at + 8;
		if (size === 1) {
			if (to - at < 16) throw new Error(`a ${type} box's 64-bit size is cut short`);
			size = Number(view.getBigUint64(at + 8));
			body = at + 16;
		} else if (size === 0) {
			size = to - at;
		}
		if (size < body - at || at + size > to) throw new Error(`a ${type} box runs past its parent`);
		out.push({ type, start: at, body, end: at + size });
		at += size;
	}
	return out;
};

const childrenOf = (data: Uint8Array, parent: Box, type: string) => boxesIn(data, parent.body, parent.end).filter((b) => b.type === type);

/** The `ftyp` + `moov` an append carries, if any: the SourceBuffer's init from then on. */
export const initOf = (data: Uint8Array, boxes: Box[]) => {
	const moov = boxes.filter((b) => b.type === "moov").at(-1);
	if (!moov) return null;
	const ftyp = boxes.filter((b) => b.type === "ftyp" && b.end <= moov.start).at(-1);
	return data.slice(ftyp?.start ?? moov.start, moov.end);
};

/** Each track's `trex` default sample duration, which a `moof` may leave its samples to. */
export const trexDurations = (init: Uint8Array) => {
	const view = new DataView(init.buffer, init.byteOffset, init.byteLength);
	const durations = new Map<number, number>();
	for (const moov of boxesIn(init).filter((b) => b.type === "moov"))
		for (const mvex of childrenOf(init, moov, "mvex"))
			for (const trex of childrenOf(init, mvex, "trex")) {
				if (trex.end - trex.body < 16) throw new Error("a trex is cut short");
				durations.set(view.getUint32(trex.body + 4), view.getUint32(trex.body + 12));
			}
	return durations;
};

export type Span = { start: number; end: number };

/** Each track's span of decode time over the append's `moof`s, in the track's timescale. */
export const decodeSpans = (media: Uint8Array, trex: Map<number, number>) => {
	const view = new DataView(media.buffer, media.byteOffset, media.byteLength);
	const spans = new Map<number, Span>();
	for (const moof of boxesIn(media).filter((b) => b.type === "moof")) {
		for (const traf of childrenOf(media, moof, "traf")) {
			const [tfhd] = childrenOf(media, traf, "tfhd");
			const [tfdt] = childrenOf(media, traf, "tfdt");
			if (!tfhd || !tfdt) throw new Error("a traf carries no tfhd or tfdt");
			const tfhdFlags = view.getUint32(tfhd.body) & 0xffffff;
			const track = view.getUint32(tfhd.body + 4);
			// Past the optional base data offset and sample description index.
			const defaultAt = tfhd.body + 8 + (tfhdFlags & 0x1 ? 8 : 0) + (tfhdFlags & 0x2 ? 4 : 0);
			const fallback = tfhdFlags & 0x8 ? view.getUint32(defaultAt) : (trex.get(track) ?? 0);
			const start = view.getUint8(tfdt.body) === 1 ? Number(view.getBigUint64(tfdt.body + 4)) : view.getUint32(tfdt.body + 4);
			let length = 0;
			for (const trun of childrenOf(media, traf, "trun")) {
				const flags = view.getUint32(trun.body) & 0xffffff;
				const count = view.getUint32(trun.body + 4);
				if (!(flags & 0x100)) {
					length += count * fallback;
					continue;
				}
				const stride = [0x100, 0x200, 0x400, 0x800].filter((f) => flags & f).length * 4;
				const first = trun.body + 8 + (flags & 0x1 ? 4 : 0) + (flags & 0x4 ? 4 : 0);
				if (first + count * stride > trun.end) throw new Error("a trun's samples run past it");
				for (let i = 0; i < count; i++) length += view.getUint32(first + i * stride);
			}
			const held = spans.get(track);
			spans.set(track, held ? { start: Math.min(held.start, start), end: Math.max(held.end, start + length) } : { start, end: start + length });
		}
	}
	return spans;
};

export const contiguous = (before: Map<number, Span>, now: Map<number, Span>) =>
	now.size > 0 && [...now].every(([track, span]) => before.get(track)?.end === span.start);
