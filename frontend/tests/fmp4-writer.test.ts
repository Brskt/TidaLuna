// Tests `src/video/fmp4-writer.ts` by parsing what it writes. A wrong base time or data offset
// raises nothing: Chromium plays the fragment at the wrong time or reads the wrong bytes.

import { expect, test } from "bun:test";

import { writeFragment } from "../src/video/fmp4-writer";

type Box = { type: string; start: number; body: number; end: number };

/** The boxes laid end to end in `bytes[from, to)`; 32-bit sizes only, as the writer emits. */
const children = (bytes: Uint8Array, from: number, to: number): Box[] => {
	const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
	const out: Box[] = [];
	for (let at = from; at < to; ) {
		const size = view.getUint32(at);
		const type = String.fromCharCode(...bytes.subarray(at + 4, at + 8));
		out.push({ type, start: at, body: at + 8, end: at + size });
		at += size;
	}
	return out;
};

const child = (bytes: Uint8Array, parent: Box, type: string) => children(bytes, parent.body, parent.end).find((b) => b.type === type)!;

test("the fragment carries its base time in tfdt and its data offset points at mdat", () => {
	// Past 32 bits: a version 0 tfdt would wrap it.
	const baseTime = 2 ** 33 + 7;
	const out = writeFragment(3, 1, baseTime, [
		{ data: new Uint8Array([1, 2, 3]), duration: 1001, key: true },
		{ data: new Uint8Array([4, 5]), duration: 1001, key: false },
	]);
	const view = new DataView(out.buffer, out.byteOffset, out.byteLength);

	const [moof, mdat] = children(out, 0, out.length) as [Box, Box];
	const traf = child(out, moof, "traf");

	const tfdt = child(out, traf, "tfdt");
	expect(out[tfdt.body]).toBe(1);
	expect(view.getBigUint64(tfdt.body + 4)).toBe(BigInt(baseTime));

	// default-base-is-moof: the trun's data offset counts from the moof's first byte.
	const tfhd = child(out, traf, "tfhd");
	expect(view.getUint32(tfhd.body) & 0xffffff).toBe(0x020000);
	const trun = child(out, traf, "trun");
	expect(view.getUint32(trun.body) & 0x1).toBe(0x1);
	const dataOffset = view.getInt32(trun.body + 8);
	expect(moof.start + dataOffset).toBe(mdat.body);
	expect([...out.subarray(moof.start + dataOffset, mdat.end)]).toEqual([1, 2, 3, 4, 5]);
});
