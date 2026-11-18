// The transcoding shim under hls.js 1.7.4's own BufferController, over a fake MediaSource that follows
// media-source-2 (synchronous `updating`, events as tasks, the spec's exceptions). An operation hls.js
// queues while the end-of-stream drain appends must run, once, or its whole queue stalls in silence.

import { expect, test } from "bun:test";
import Hls from "hls.js/light";

// Imported here, not per test: it installs the emit hook the harness would delete after a test.
import "../src/ipc";
import { writeFragment, writeInit } from "../src/video/fmp4-writer";
import { audioRecord, codec, type FakeMediaSource, fakeWebCodecs, loadShimOverFakes, log, lvf1, serve } from "./helpers/transcode-shim-fakes";

const { installTranscodingMediaSource } = await loadShimOverFakes();

const init = writeInit([{ kind: "audio", id: 1, timescale: 44100, channels: 2, preSkip: 0 }]);
const media = writeFragment(1, 1, 0, [{ data: new Uint8Array([0x21, 0x10]), duration: 1024, key: true }]);

const timerTurns = async (n: number) => {
	for (let i = 0; i < n; i++) await new Promise((resolve) => setTimeout(resolve, 0));
};

/** As much of `<video>` as hls.js's attach and BufferController read. */
class FakeMedia extends EventTarget {
	src = "";
	currentSrc = "";
	currentTime = 0;
	duration = 10;
	seeking = false;
	paused = true;
	error = null;
	readyState = 0;
	disableRemotePlayback = false;
	removeAttribute() {}
	load() {}
	querySelector() {
		return null;
	}
	querySelectorAll() {
		return [];
	}
	appendChild() {}
	removeChild() {}
}

const FLUSH_ALL = { startOffset: 0, endOffset: Number.POSITIVE_INFINITY, type: null };

/** hls.js over the shim with one audio SourceBuffer, an init and a fragment in. */
const hlsSetup = async () => {
	codec("ready");
	// The Opus encoder keeps its latest packet until `flush()`, as Chromium's does.
	fakeWebCodecs({ hold: true });
	serve(lvf1(audioRecord(0), audioRecord(1024)));
	(globalThis as any).self ??= globalThis;
	(URL as any).createObjectURL = () => "blob:fake";
	(URL as any).revokeObjectURL = () => {};
	installTranscodingMediaSource();
	const hls = new Hls({ enableWorker: false, autoStartLoad: false, appendTimeout: Infinity } as any);
	const bc = (hls as any).bufferController;
	const hlsEvents: string[] = [];
	const errors: unknown[] = [];
	for (const name of ["BUFFER_APPENDED", "BUFFER_FLUSHED"] as const) hls.on(Hls.Events[name], () => hlsEvents.push(name));
	hls.on(Hls.Events.ERROR, (_e, data) => errors.push({ details: data.details, fatal: data.fatal }));
	hls.attachMedia(new FakeMedia() as unknown as HTMLMediaElement);
	const ms = bc.mediaSource as FakeMediaSource;
	ms.open();
	ms.duration = 10;
	hls.trigger(Hls.Events.MANIFEST_PARSED, { levels: [], audioTracks: [], subtitleTracks: [], firstLevel: 0, stats: {}, audio: true, video: false, altAudio: false } as any);
	hls.trigger(Hls.Events.BUFFER_CODECS, { audio: { id: "main", container: "audio/mp4", codec: "mp4a.40.2", levelCodec: undefined } } as any);
	const sb = bc.tracks.audio.buffer as SourceBuffer;
	const real = ms.sourceBuffers.at(-1)!;
	const page: string[] = [];
	for (const type of ["updatestart", "update", "updateend", "error", "abort"]) sb.addEventListener(type, () => page.push(type));
	let sn = 0;
	const append = (data: Uint8Array<ArrayBuffer>, isInit = false) => {
		const frag = { sn: isInit ? "initSegment" : ++sn, cc: 0, level: 0, type: "main", start: 0, end: 2, duration: 2, gap: false, stats: { buffering: { start: 0, first: 0, end: 0 } }, elementaryStreams: {} };
		const chunkMeta = { buffering: { audio: { start: 0, executeStart: 0, executeEnd: 0, end: 0 } }, id: 1, sn: frag.sn, level: 0, partial: false, iframe: false };
		hls.trigger(Hls.Events.BUFFER_APPENDING, { type: "audio", data, frag, part: null, chunkMeta, parent: "main", offset: undefined } as any);
	};
	const queued = () => bc.operationQueue.queues.audio.length as number;
	append(init, true);
	await timerTurns(20);
	append(media);
	await timerTurns(40);
	expect(errors).toEqual([]);
	page.length = 0;
	hlsEvents.length = 0;
	log.length = 0;
	return { hls, ms, sb, real, page, hlsEvents, errors, append, queued };
};

type Setup = Awaited<ReturnType<typeof hlsSetup>>;

/** hls.js ends the stream; returns once the drain's own append is on the real SourceBuffer. */
const drainInFlight = async (s: Setup) => {
	s.real.hold = true;
	s.hls.trigger(Hls.Events.BUFFER_EOS, {} as any);
	for (let i = 0; i < 400 && !s.real.updating; i++) await Promise.resolve();
	s.real.hold = false;
	expect({ real: s.real.updating, page: s.sb.updating }).toEqual({ real: true, page: false });
};

test("a seek back while the drain appends: hls.js's flush and next append run, once each", async () => {
	const s = await hlsSetup();
	await drainInFlight(s);
	s.hls.trigger(Hls.Events.BUFFER_FLUSHING, FLUSH_ALL as any);
	s.append(media);
	s.real.release();
	await timerTurns(120);
	expect(s.errors).toEqual([]);
	expect(s.queued()).toBe(0);
	expect(s.hlsEvents).toEqual(["BUFFER_FLUSHED", "BUFFER_APPENDED"]);
	expect(s.page.filter((e) => e === "updatestart")).toHaveLength(2);
	expect(s.page.filter((e) => e === "updateend")).toHaveLength(2);
});

test("an append while the drain appends runs, with one set of events", async () => {
	const s = await hlsSetup();
	await drainInFlight(s);
	s.append(media);
	s.real.release();
	await timerTurns(120);
	expect(s.errors).toEqual([]);
	expect(s.queued()).toBe(0);
	expect(s.hlsEvents).toEqual(["BUFFER_APPENDED"]);
	expect(s.page).toEqual(["updatestart", "update", "updateend"]);
});

test("after a drain a seek superseded, hls.js's next end drains the new fragment's tail", async () => {
	const s = await hlsSetup();
	await drainInFlight(s);
	s.hls.trigger(Hls.Events.BUFFER_FLUSHING, FLUSH_ALL as any);
	s.append(media);
	s.real.release();
	await timerTurns(100);
	const before = s.real.appends.length;
	s.hls.trigger(Hls.Events.BUFFER_EOS, {} as any);
	await timerTurns(100);
	expect(s.ms.readyState).toBe("ended");
	expect(s.real.appends.length).toBe(before + 1);
	expect(s.errors).toEqual([]);
});
