// Tests for `askCodec` in `src/video/codec-state.ts`.
//
// A clip asks the backend for the codec and takes its answer, for at most the bound it is given: a
// held failure answers at once with its reason, a retry is awaited, a stopped service refuses, and
// at the bound the snapshot speaks while the backend's step goes on.

import { expect, mock, test } from "bun:test";

// Imported here, not per test: it installs the emit hook the harness would delete after a test.
const ipc = await import("../src/ipc");

let reply: () => Promise<unknown> = () => new Promise(() => {});
// Only the ask is scripted; the pull never answers, as the harness's `cefQuery` never does.
mock.module("../src/ipc", () => ({
	...ipc,
	invokeIpc: (channel: string) => (channel === "settings.video_codec_ask" ? reply() : new Promise(() => {})),
}));

const view = (seq: number, phase: Record<string, unknown>) => ({ seq, enabled: true, phase });
const emit = (seq: number, phase: Record<string, unknown>) => (window as any).__LUNAR_IPC_EMIT__("settings.video_codec_state", view(seq, phase));
const BOUND = 5000;

// A query string gives each test its own module instance; no snapshot carries over.
const fresh = async (tag: string) => {
	const mod = await import(`../src/video/codec-state?${tag}`);
	mod.watchCodecState();
	return mod;
};

test("askCodec waits for the step while the bound has not passed", async () => {
	const { askCodec } = await fresh("downloading");
	emit(1, { kind: "downloading" });
	let finish!: () => void;
	reply = () => new Promise((resolve) => (finish = () => resolve(view(2, { kind: "ready" }))));
	let settled = false;
	const answer = askCodec(BOUND).then((a: unknown) => ((settled = true), a));
	await new Promise((resolve) => setTimeout(resolve, 30));
	expect(settled).toBe(false);
	finish();
	expect(await answer).toEqual({ ready: true });
});

test("askCodec answers a held failure at once with its reason", async () => {
	const { askCodec } = await fresh("held");
	reply = async () => view(3, { kind: "failed", reason: "the codec download was refused", recovery: "on_demand" });
	expect(await askCodec(BOUND)).toEqual({ ready: false, refusal: "the codec download was refused" });
});

test("askCodec carries the text of a rejected ask", async () => {
	const { askCodec } = await fresh("rejected");
	reply = () => Promise.reject(Object.assign(new Error("the video codec service has stopped"), { code: 503 }));
	expect(await askCodec(BOUND)).toEqual({ ready: false, refusal: "the video codec service has stopped" });
});

test("a push newer than the answer is the state the answer is read against", async () => {
	const { askCodec } = await fresh("newer");
	emit(9, { kind: "ready" });
	reply = async () => view(8, { kind: "downloading" });
	expect(await askCodec(BOUND)).toEqual({ ready: true });
});

test("at the bound the snapshot answers, and the late reply only updates the snapshot", async () => {
	const { askCodec, codecReady } = await fresh("bound");
	emit(1, { kind: "downloading" });
	let late!: () => void;
	reply = () => new Promise((resolve) => (late = () => resolve(view(2, { kind: "ready" }))));
	const answer = await askCodec(20);
	expect(answer).toEqual({ ready: false, refusal: "the video codec is not ready yet" });
	expect(codecReady()).toBe(false);

	late();
	await new Promise((resolve) => setTimeout(resolve, 0));
	// What the asker was told stands; the next one reads ready.
	expect(codecReady()).toBe(true);
});
