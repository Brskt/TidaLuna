// Tests `src/video/transcode-shim.ts` against a fake real MediaSource, `luna-cover://` and WebCodecs
// (happy-dom has none). A second `updateend`, a missing one or an offset applied early stalls or
// drifts the clip without any error.

import { expect, test } from "bun:test";

// Imported here, not per test: it installs the emit hook the harness would delete after a test.
import "../src/ipc";
import { writeFragment, writeInit } from "../src/video/fmp4-writer";
import { audioRecord, codec, type FakeMediaSource, fakeWebCodecs, loadShimOverFakes, log, lvf1, serve, videoRecord } from "./helpers/transcode-shim-fakes";
import type { SentIpc } from "./setup";

const { installTranscodingMediaSource, shimIsTypeSupported } = await loadShimOverFakes();

/** The input an hls.js SourceBuffer receives: any init, then one media fragment of one sample. */
const videoInit = writeInit([{ kind: "video", id: 1, timescale: 90000, width: 2, height: 2 }]);
const videoMedia = writeFragment(1, 1, 0, [{ data: new Uint8Array([0, 0, 0, 1, 0x65]), duration: 3000, key: true }]);
const audioInit = writeInit([{ kind: "audio", id: 1, timescale: 44100, channels: 2, preSkip: 0 }]);
const audioMedia = writeFragment(1, 1, 0, [{ data: new Uint8Array([0x21, 0x10]), duration: 1024, key: true }]);

const shimBuffer = (type: string) => {
	installTranscodingMediaSource();
	const ms = new (globalThis as any).MediaSource() as FakeMediaSource;
	ms.open();
	ms.duration = 10;
	const sb = (ms as unknown as MediaSource).addSourceBuffer(type);
	return { sb, real: ms.sourceBuffers.at(-1)!, ms };
};

/** Each `moof`'s track id and `tfdt` base in a real append, read with the writer's own layout. */
const baseTimes = (bytes: Uint8Array) => {
	const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
	const boxes = (from: number, to: number) => {
		const out: { type: string; body: number; end: number }[] = [];
		for (let at = from; at < to; at += view.getUint32(at)) {
			out.push({ type: String.fromCharCode(...bytes.subarray(at + 4, at + 8)), body: at + 8, end: at + view.getUint32(at) });
		}
		return out;
	};
	return boxes(0, bytes.length)
		.filter((b) => b.type === "moof")
		.flatMap((moof) => boxes(moof.body, moof.end).filter((b) => b.type === "traf"))
		.map((traf) => {
			const inner = boxes(traf.body, traf.end);
			const tfhd = inner.find((b) => b.type === "tfhd")!;
			const tfdt = inner.find((b) => b.type === "tfdt")!;
			return { track: view.getUint32(tfhd.body + 4), base: Number(view.getBigUint64(tfdt.body + 4)) };
		});
};

const record = (sb: SourceBuffer) => {
	const events: string[] = [];
	for (const type of ["updatestart", "update", "updateend", "error", "abort"]) sb.addEventListener(type, () => events.push(type));
	return events;
};

const appended = (sb: SourceBuffer, bytes: Uint8Array<ArrayBuffer>) =>
	new Promise<void>((resolve) => {
		sb.addEventListener("updateend", () => resolve(), { once: true });
		sb.appendBuffer(bytes);
	});

test("fires exactly one updatestart and one updateend per append", async () => {
	codec("ready");
	fakeWebCodecs();
	serve(lvf1(videoRecord(0, 3000)));
	const { sb, real } = shimBuffer('video/mp4; codecs="avc1.640028"');
	const events = record(sb);

	await appended(sb, videoInit);
	await appended(sb, videoMedia);
	// The real SourceBuffer's own events, still queued past the shim's, must not reach it either.
	await new Promise((resolve) => setTimeout(resolve, 10));

	expect(real.appends).toHaveLength(1);
	expect(events.filter((e) => e === "updatestart")).toHaveLength(2);
	expect(events.filter((e) => e === "updateend")).toHaveLength(2);
	expect(events).not.toContain("error");
});

// The watchdog is the deadline the app published when the shim installed, the one Rust decodes to.
test("reports error then updateend when the transcode never answers", async () => {
	codec("ready");
	fakeWebCodecs();
	globalThis.fetch = (() => new Promise(() => {})) as unknown as typeof fetch;
	(window as any).__TIDALUNAR_FRAGMENT_DEADLINE_MS__ = 20;
	const { sb } = shimBuffer('video/mp4; codecs="avc1.640028"');
	const events = record(sb);
	await appended(sb, videoMedia);
	expect(events).toEqual(["updatestart", "error", "updateend"]);
	expect(sb.updating).toBe(false);
});

test("answers isTypeSupported for avc1 and mp4a.40.2 only while the codec is ready", () => {
	const sent = (globalThis as any).__LUNAR_TEST_IPC__ as SentIpc[];
	const demands = () => sent.filter((s) => s.channel === "settings.video_codec_demand").length;
	const avc = 'video/mp4; codecs="avc1.640028"';
	const aac = 'audio/mp4; codecs="mp4a.40.2"';

	codec("downloading");
	const before = demands();
	expect([shimIsTypeSupported(avc), shimIsTypeSupported(aac)]).toEqual([false, false]);
	// A player told no is demand: a codec whose download failed is fetched again.
	expect(demands()).toBe(before + 2);

	codec("ready");
	expect(shimIsTypeSupported(avc)).toBe(true);
	expect(shimIsTypeSupported(aac)).toBe(true);
});

test("applies timestampOffset at the real append, not when it is set", async () => {
	codec("ready");
	fakeWebCodecs();
	serve(lvf1(audioRecord(0)));
	const { sb, real } = shimBuffer('audio/mp4; codecs="mp4a.40.2"');

	// hls.js's TS path rebases each fragment this way, before the append it applies to.
	sb.timestampOffset = -1.4;
	expect(real.timestampOffset).toBe(0);
	await appended(sb, audioInit);
	await appended(sb, audioMedia);

	expect(real.appends.map((a) => a.offset)).toEqual([-1.4]);
	expect(sb.timestampOffset).toBe(-1.4);
});

test("appends the audio the encoder still holds before ending the stream", async () => {
	codec("ready");
	fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024)));
	const { sb, ms } = shimBuffer('audio/mp4; codecs="mp4a.40.2"');
	await appended(sb, audioInit);
	const events = record(sb);
	log.length = 0;

	await appended(sb, audioMedia);
	(ms as unknown as MediaSource).endOfStream();
	for (let tick = 0; tick < 50 && ms.readyState !== "ended"; tick++) await new Promise((resolve) => setTimeout(resolve, 0));

	// The packet already out, then the held tail, and only then the real end of stream.
	expect(log).toEqual(["append", "append", "eos"]);
	// The tail is the shim's own append: a second `updateend` would drop hls.js's next append.
	expect(events).toEqual(["updatestart", "update", "updateend"]);
	expect(ms.readyState).toBe("ended");
});

test("cuts the audio an AAC edit list puts before zero instead of writing a negative base", async () => {
	codec("ready");
	let seen = fakeWebCodecs();
	// Rust applies the edit list as Chromium's parser does: the first frames land before 0.
	serve(lvf1(audioRecord(-512), audioRecord(512), videoRecord(-3000, 3000), videoRecord(0, 3000)));
	const muxed = shimBuffer('video/mp4; codecs="avc1.64001f,mp4a.40.2"');
	await appended(muxed.sb, audioInit);
	await appended(muxed.sb, audioMedia);

	// Cut at 0: the 512 frames after it keep their place, the rest of the AAC frame is gone.
	expect(seen.audioData[0]).toEqual({ timestamp: 0, duration: Math.round((512 / 44100) * 1e6) });
	// A frame wholly before 0 is dropped; the next one opens the fragment as its keyframe.
	expect(seen.videoFrames).toEqual([{ timestamp: 0, key: true }]);
	expect(baseTimes(muxed.real.appends[0]!.bytes)).toEqual([
		{ track: 1, base: 0 },
		{ track: 2, base: 0 },
	]);

	// A record wholly before 0 is not encoded, and the next one still follows on the same encoder.
	seen = fakeWebCodecs();
	serve(lvf1(audioRecord(-1024), audioRecord(0)));
	const audio = shimBuffer('audio/mp4; codecs="mp4a.40.2"');
	await appended(audio.sb, audioInit);
	await appended(audio.sb, audioMedia);

	expect(seen.audioData).toEqual([{ timestamp: 0, duration: Math.round((1024 / 44100) * 1e6) }]);
	expect(seen.audioEncoders).toBe(1);
	expect(baseTimes(audio.real.appends[0]!.bytes)).toEqual([{ track: 2, base: 0 }]);
});

const timerTurns = (n: number) =>
	new Promise<void>((resolve) => {
		let turn = 0;
		const next = () => (++turn >= n ? resolve() : setTimeout(next, 0));
		next();
	});

/** hls.js 1.7.4's media detach (buffer-controller.ts:543-564): abort and remove each listed buffer, then end the stream. */
const hlsDetach = (ms: MediaSource) => {
	const sourceBuffers = ms.sourceBuffers;
	const open = ms.readyState === "open";
	for (let i = sourceBuffers.length; i--;) {
		if (open) sourceBuffers[i].abort();
		ms.removeSourceBuffer(sourceBuffers[i]);
	}
	try {
		if (open) ms.endOfStream();
	} catch {
		// hls.js only warns on it.
	}
};

const MUXED = 'video/mp4; codecs="avc1.64001f,mp4a.40.2"';

test("hls.js's detach removes the listed real buffer and still detaches its shim", async () => {
	codec("ready");
	fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024), videoRecord(0, 3000)));
	const { sb, ms } = shimBuffer(MUXED);
	await appended(sb, audioInit);
	await appended(sb, audioMedia);
	const shim = sb as any;
	expect(shim.holdsAudio).toBe(true);

	hlsDetach(ms as unknown as MediaSource);
	await timerTurns(50);

	expect({ shimRemoved: shim.removed, shimStillListed: (ms as any).shims.has(shim) }).toEqual({ shimRemoved: true, shimStillListed: false });
});

test("hls.js's detach during a fragment aborts the transcode request", async () => {
	codec("ready");
	const seen = fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024), videoRecord(0, 3000)));
	const { sb, ms } = shimBuffer(MUXED);
	await appended(sb, audioInit);
	await appended(sb, audioMedia);

	let signal: AbortSignal | undefined;
	globalThis.fetch = ((_url: unknown, init?: RequestInit) => {
		signal = init?.signal ?? undefined;
		return new Promise(() => {});
	}) as unknown as typeof fetch;
	const fired: string[] = [];
	const handler = (e: Event) => fired.push(e.type);
	sb.addEventListener("updateend", handler);
	sb.addEventListener("abort", handler);
	sb.appendBuffer(audioMedia);
	await timerTurns(5);
	expect(signal?.aborted).toBe(false);

	// hls.js takes its listeners off in the same synchronous detach: nothing may reach them after.
	hlsDetach(ms as unknown as MediaSource);
	sb.removeEventListener("updateend", handler);
	sb.removeEventListener("abort", handler);
	await timerTurns(20);

	expect({ signalAborted: signal?.aborted, encoders: seen.encoders.map((e) => e.state), eventsToHlsListeners: fired }).toEqual({
		signalAborted: true,
		encoders: ["closed", "closed"],
		eventsToHlsListeners: [],
	});
});

// The end-of-stream drain appends on the real SourceBuffer on its own: the page's operations that
// land meanwhile must neither throw nor wait on an `updateend` it never sees.

const microtaskTurns = async (n: number) => {
	for (let i = 0; i < n; i++) await Promise.resolve();
};

/** An EOS drain with its real append held in flight; returns once that append is on the real buffer. */
const drainInFlight = async (before?: (sb: SourceBuffer) => void) => {
	codec("ready");
	fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024)));
	const { sb, real, ms } = shimBuffer('audio/mp4; codecs="mp4a.40.2"');
	await appended(sb, audioInit);
	await appended(sb, audioMedia);
	before?.(sb);
	log.length = 0;
	const events = record(sb);
	real.hold = true;
	(ms as unknown as MediaSource).endOfStream();
	for (let i = 0; i < 400 && !real.updating; i++) await microtaskTurns(1);
	expect(real.updating).toBe(true);
	real.hold = false;
	return { sb, real, ms, events };
};

test("an append contiguous with the drained audio keeps its encoder, and the next end drains its tail", async () => {
	const { sb, real, ms } = await drainInFlight();
	// Contiguous with the first fragment (its audio ended at 2048).
	serve(lvf1(audioRecord(2048), audioRecord(3072)));
	sb.appendBuffer(audioMedia);
	real.release();
	await timerTurns(60);
	expect((sb as any).holdsAudio).toBe(true);
	const appendsBefore = real.appends.length;

	(ms as unknown as MediaSource).endOfStream();
	await timerTurns(60);
	expect(real.appends.length).toBe(appendsBefore + 1);
	expect({ last: log.at(-1), state: ms.readyState, holds: (sb as any).holdsAudio }).toEqual({ last: "eos", state: "ended", holds: false });
});

test("changeType during the drain's append is accepted", async () => {
	const { sb, real } = await drainInFlight();
	sb.changeType('audio/mp4; codecs="mp4a.40.2"');
	await timerTurns(30);
	expect(real.changeTypes).toHaveLength(0);
	sb.changeType('video/mp4; codecs="avc1.640028"');
	expect(real.changeTypes).toHaveLength(1);
});

test("abort() during the drain's append raises nothing for the page and ends nothing", async () => {
	const { sb, ms, events } = await drainInFlight();
	sb.abort();
	await timerTurns(30);
	expect(events).toEqual([]);
	expect(ms.readyState).toBe("open");
	expect(sb.updating).toBe(false);
});

test("a second endOfStream during the drain's flush is the same end", async () => {
	codec("ready");
	fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024)));
	const { sb, ms } = shimBuffer('audio/mp4; codecs="mp4a.40.2"');
	await appended(sb, audioInit);
	await appended(sb, audioMedia);
	log.length = 0;
	(ms as unknown as MediaSource).endOfStream();
	(ms as unknown as MediaSource).endOfStream();
	await timerTurns(60);
	expect({ log, state: ms.readyState }).toEqual({ log: ["append", "eos"], state: "ended" });
});

test("removing the buffer during the drain's append ends nothing and tells the page nothing", async () => {
	const { sb, ms, events } = await drainInFlight();
	hlsDetach(ms as unknown as MediaSource);
	await timerTurns(30);
	expect({ events: [...events], removed: (sb as any).removed }).toEqual({ events: [], removed: true });
	// hls.js's detach ends the stream itself; the cancelled drain ends nothing more.
	expect(log.filter((l) => l === "eos")).toHaveLength(1);
});

test("a remove waits for the drain's append: the tail is kept and nothing is aborted", async () => {
	const { sb, real } = await drainInFlight();
	sb.remove(0, 1);
	expect(real.removes).toHaveLength(0);
	real.release();
	await timerTurns(30);
	expect(log).toEqual(["append", "remove"]);
	expect(real.removes).toEqual([[0, 1]]);
});

test("a call that cannot wait aborts the drain's append and leaves the page's append window alone", async () => {
	const { sb, real } = await drainInFlight((sb) => {
		sb.appendWindowStart = 2;
		sb.appendWindowEnd = 9;
	});
	sb.changeType('video/mp4; codecs="avc1.640028"');
	await timerTurns(30);
	expect(log).toContain("real-abort");
	expect({ start: real.appendWindowStart, end: real.appendWindowEnd, changed: real.changeTypes.length }).toEqual({ start: 2, end: 9, changed: 1 });
});

test("a page abort() still resets the append window", async () => {
	const { sb, real } = await drainInFlight();
	sb.appendWindowStart = 2;
	sb.abort();
	expect(real.appendWindowStart).toBe(0);
});

test("abort() while a remove waits behind the drain's append ends it as aborted, once", async () => {
	const { sb, real, events } = await drainInFlight();
	sb.remove(0, 1);
	expect(() => sb.appendBuffer(audioMedia)).toThrow("still processing");
	await microtaskTurns(5);
	sb.abort();
	real.release();
	await timerTurns(40);
	expect(events).toEqual(["updatestart", "abort", "updateend"]);
	expect(real.removes).toHaveLength(0);
	expect(sb.updating).toBe(false);
});

test("removeSourceBuffer while a remove waits behind the drain's append ends it as aborted and removes nothing", async () => {
	const { sb, real, ms, events } = await drainInFlight();
	sb.remove(0, 1);
	await microtaskTurns(5);
	hlsDetach(ms as unknown as MediaSource);
	real.release();
	await timerTurns(40);
	expect(events).toEqual(["updatestart", "abort", "updateend"]);
	expect(real.removes).toHaveLength(0);
});

test("a remove the real SourceBuffer refuses, behind the drain's append, ends as error", async () => {
	const { sb, real, events } = await drainInFlight();
	(real as any).remove = () => {
		throw new DOMException("bad", "InvalidStateError");
	};
	sb.remove(0, 1);
	real.release();
	await timerTurns(40);
	expect(events).toEqual(["updatestart", "error", "updateend"]);
	expect(sb.updating).toBe(false);
});

test("the watchdog of a page append stuck behind the drain's append aborts it and frees the buffer", async () => {
	(window as any).__TIDALUNAR_FRAGMENT_DEADLINE_MS__ = 20;
	const { sb, real, events } = await drainInFlight();
	sb.appendBuffer(audioMedia);
	await timerTurns(60);
	expect(events).toEqual(["updatestart", "error", "updateend"]);
	expect(log).toContain("real-abort");
	expect(sb.updating).toBe(false);
	// The buffer works again once the abandoned append's events are in.
	serve(lvf1(audioRecord(0)));
	const after = record(sb);
	await appended(sb, audioMedia);
	expect(after).toEqual(["updatestart", "update", "updateend"]);
	expect(real.updating).toBe(false);
});
