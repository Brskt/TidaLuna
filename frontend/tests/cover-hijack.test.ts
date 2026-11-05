// Tests `src/video/cover-hijack.ts` through the hooks `src/audio-proxy.ts` installs: TIDAL's cover
// `<source>` is pointed at our transcode, a miss transcodes it once, and every element showing it
// reloads with a URL the media stack has not seen fail.
//
// happy-dom fires no media events and loads nothing: a miss is dispatched by hand, as
// `early-runtime/cover_error.js` dispatches it.

import { afterEach, expect, jest, setSystemTime, test } from "bun:test";

import "../src/audio-proxy";
import { coverQueueSettled, setTranscoderForTests } from "../src/video/cover-hijack";
import type { TranscodeOutcome } from "../src/video/cover-encode";

const real = (name: string, variant = 1280) =>
	`https://resources.tidal.com/videos/${name}/${variant}x${variant}.mp4`;
const ours = (src: string, edge: number) => `luna-cover://video/?src=${encodeURIComponent(src)}&edge=${edge}`;

/** A layout box of `css` px on `el`: happy-dom lays nothing out. */
const boxed = (el: Element, css: number) => {
	Object.defineProperty(el, "clientWidth", { configurable: true, get: () => css });
	Object.defineProperty(el, "clientHeight", { configurable: true, get: () => css });
};

afterEach(() => {
	document.body.replaceChildren();
	jest.useRealTimers();
	setSystemTime();
});

/** A transcoder that answers `outcome` and counts its calls. */
const transcoder = (outcome: TranscodeOutcome) => {
	const calls: string[] = [];
	setTranscoderForTests(async (src, edge) => {
		calls.push(`${src}|${edge}`);
		return outcome;
	});
	return calls;
};

/** TIDAL's playback view: a `<video width=1280>`, its `<source>` written by React, then `load()`. */
const mount = (src: string, width: string | null = "1280") => {
	const video = document.createElement("video");
	if (width) video.setAttribute("width", width);
	const source = document.createElement("source");
	source.setAttribute("src", src);
	video.appendChild(source);
	document.body.appendChild(video);
	video.load();
	return { video, source };
};

/** As `cover_error.js` sends it, by default for the request the element holds now. */
const miss = (target: Element, url?: string) => {
	const source = target instanceof HTMLVideoElement ? target.querySelector("source")! : (target as HTMLSourceElement);
	window.dispatchEvent(new CustomEvent("luna-cover-miss", { detail: { target, url: url ?? source.src } }));
};

/** A miss, then the queue drained. */
const missed = async (target: Element, url?: string) => {
	miss(target, url);
	await coverQueueSettled();
};

// React writes the attribute before the child has a parent: the default edge, until `load()`.
test("a cover source written before its video takes the video's edge at load", () => {
	const source = document.createElement("source");
	source.setAttribute("src", real("attr"));
	expect(source.getAttribute("src")).toBe(ours(real("attr"), 640));
	const video = document.createElement("video");
	video.setAttribute("width", "1280");
	video.appendChild(source);
	video.load();
	expect(source.getAttribute("src")).toBe(ours(real("attr"), 1280));
});

test("a miss transcodes the cover once and reloads every element with a URL that never failed", async () => {
	const calls = transcoder("stored");
	const first = mount(real("shared"));
	const second = mount(real("shared", 640));

	// Both elements fail; one transcode serves both.
	miss(first.source);
	await missed(second.video);

	expect(calls).toEqual([`${real("shared")}|1280`]);
	// Each keeps the variant TIDAL named: Rust keys the cover without it.
	expect(first.source.getAttribute("src")!.startsWith(`${ours(real("shared"), 1280)}&r=`)).toBe(true);
	expect(second.source.getAttribute("src")!.startsWith(`${ours(real("shared", 640), 1280)}&r=`)).toBe(true);
	// A later `load()` of TIDAL's keeps the bust, never the URL that failed.
	const reloaded = first.source.getAttribute("src");
	first.video.load();
	expect(first.source.getAttribute("src")).toBe(reloaded);
});

test("a transcode that fails is tried twice per cover, then left to TIDAL's poster", async () => {
	const calls = transcoder("failed");
	const { source } = mount(real("refused"));
	for (let i = 0; i < 5; i++) await missed(source);
	expect(calls.length).toBe(2);
});

// Without the latch every cover shown would download and decode again, for an encoder that is not there.
test("a missing encoder leaves covers still for the session", async () => {
	const none = transcoder("no-encoder");
	const b = mount(real("none-b"));
	const c = mount(real("none-c"));
	await missed(b.source);
	miss(c.source);
	await missed(b.source);
	expect(none).toEqual([`${real("none-b")}|1280`]);
});

/** `ms` later on the fake clock, then a miss on `target` and the queue drained. */
const missAfter = async (ms: number, target: Element) => {
	jest.advanceTimersByTime(ms);
	await coverQueueSettled();
	await missed(target);
};

// Two Clears (or evictions) in a session must not leave a cover still for good.
test("a cover the cache lost is transcoded again, however often", async () => {
	jest.useFakeTimers();
	const calls = transcoder("stored");
	const { source } = mount(real("cleared"));
	for (let i = 0; i < 4; i++) await missAfter(60_000, source);
	expect(calls.length).toBe(4);
	// A store climbs too: a file that loads then breaks does not come back as one pass per miss.
	await missAfter(0, source);
	expect(calls.length).toBe(4);
});

// Nothing counts a pass the machine failed: the ladder alone stops it from becoming one per miss.
test("a pass the machine failed spends nothing, and the next one waits twice as long", async () => {
	jest.useFakeTimers();
	const calls = transcoder("retry-on-miss");
	const { source } = mount(real("ladder"));
	await missAfter(0, source); // 1, floor 5 s
	await missAfter(5_000, source); // 2, floor 10 s
	await missAfter(2_000, source); // in the floor: put off to its end
	expect(calls.length).toBe(2);
	jest.advanceTimersByTime(8_000);
	await coverQueueSettled();
	expect(calls.length).toBe(3); // the put-off pass, floor 20 s
	await missAfter(1_000, source); // in the floor again: dropped, once per calm spell
	jest.advanceTimersByTime(19_000);
	await coverQueueSettled();
	expect(calls.length).toBe(3);
	await missAfter(0, source);
	expect(calls.length).toBe(4);
});

test("a cover Rust could not serve yet is asked again only while it is on screen", async () => {
	jest.useFakeTimers();
	const calls = transcoder("retry-soon");
	const { source } = mount(real("soon"));
	await missAfter(0, source);
	jest.advanceTimersByTime(5_000);
	await coverQueueSettled();
	expect(calls.length).toBe(2);
	document.body.replaceChildren();
	jest.advanceTimersByTime(60 * 60_000);
	await coverQueueSettled();
	expect(calls.length).toBe(2);
});

// A floor set before the clock went back would otherwise hold the cover still for hours.
test("a clock set back does not hold a cover in its floor", async () => {
	jest.useFakeTimers();
	const calls = transcoder("retry-on-miss");
	const { source } = mount(real("clock"));
	await missAfter(0, source);
	setSystemTime(Date.now() - 2 * 60 * 60_000);
	await missAfter(0, source);
	expect(calls.length).toBe(2);
	// And the ladder starts over from its first rung.
	await missAfter(5_000, source);
	expect(calls.length).toBe(3);
});

// A plugin's `load()` wrapper is foreign code: its throw must cost only its own element.
test("a reload that throws costs only its own element", async () => {
	transcoder("stored");
	const first = mount(real("wrapped"));
	const second = mount(real("wrapped"));
	first.video.load = () => {
		throw new Error("foreign wrapper");
	};
	const errors = jest.spyOn(console, "error").mockImplementation(() => {});
	try {
		await missed(first.source);
		expect(second.source.getAttribute("src")!.includes("&r=")).toBe(true);
		expect(errors.mock.calls.some((c) => String(c[1]).includes("foreign wrapper"))).toBe(true);
	} finally {
		errors.mockRestore();
	}
});

// Whatever a pass throws, from wherever: the queue is a serializer, not the pass's outcome.
test("a pass that throws outside any reload does not stop the covers behind it", async () => {
	const calls = transcoder("stored");
	const a = mount(real("broken"));
	const b = mount(real("after"));
	let armed = false;
	const record = (a.source as any)._lunaCover;
	Object.defineProperty(a.source, "_lunaCover", {
		configurable: true,
		get() {
			if (armed) {
				armed = false;
				throw new Error("pass broke");
			}
			return record;
		},
		set() {},
	});
	const errors = jest.spyOn(console, "error").mockImplementation(() => {});
	try {
		miss(a.source);
		miss(b.source);
		armed = true;
		await coverQueueSettled();
		expect(calls).toEqual([`${real("broken")}|1280`, `${real("after")}|1280`]);
		expect(b.source.getAttribute("src")!.includes("&r=")).toBe(true);
		expect(errors.mock.calls.some((c) => String(c[1]).includes("pass broke"))).toBe(true);
	} finally {
		errors.mockRestore();
	}
});

// React rewrites the `<source>` before its `load()`: the old request's error can run in between.
test("a miss of a request the element no longer holds transcodes nothing", async () => {
	const calls = transcoder("stored");
	const { source } = mount(real("left"));
	const failed = source.src;
	source.setAttribute("src", real("shown"));
	const warns = jest.spyOn(console, "warn").mockImplementation(() => {});
	try {
		await missed(source, failed);
		expect(calls).toEqual([]);
		// The request it holds now reports its own miss, and that one is acted on.
		await missed(source);
		expect(calls).toEqual([`${real("shown")}|1280`]);
	} finally {
		warns.mockRestore();
	}
});

// A box that differs at every miss reloads up at most once per size, then transcodes.
test("a box that differs between two misses does not reload without bound", async () => {
	const calls = transcoder("stored");
	const { video, source } = mount(real("flip"), null);
	let css = 722;
	Object.defineProperty(video, "clientWidth", { configurable: true, get: () => css });
	let loads = 0;
	const load = video.load;
	video.load = () => {
		loads++;
		load.call(video);
	};
	for (let i = 0; i < 20; i++) {
		css = i % 2 ? 722 : 240;
		await missed(source);
	}
	expect(loads).toBeLessThanOrEqual(4);
	expect(calls.length).toBeGreaterThan(0);
});

// Asked for larger, a size that fails for good must not leave the element worse than without it.
test("a size that failed twice is left for the next size down that can play", async () => {
	const calls: number[] = [];
	setTranscoderForTests(async (_src, edge) => {
		calls.push(edge);
		return edge === 1280 ? "failed" : "stored";
	});
	const { video, source } = mount(real("closed"), null);
	boxed(video, 722);
	video.load();
	expect(source.getAttribute("src")).toBe(ours(real("closed"), 1280));
	await missed(source);
	await missed(source);
	// Two failed passes close 1280: the element is reloaded at 640, and that one transcodes.
	expect(source.getAttribute("src")).toBe(ours(real("closed"), 640));
	await missed(source);
	expect(calls).toEqual([1280, 1280, 640]);
	expect(source.getAttribute("src")!.startsWith(`${ours(real("closed"), 640)}&r=`)).toBe(true);
});

// TIDAL keeps its `<video>` across albums: the size reloaded up for one cover says nothing of the next.
test("a cover written into a reused video is reloaded up before it is transcoded", async () => {
	const calls: string[] = [];
	setTranscoderForTests(async (src, edge) => {
		calls.push(`${src.split("/")[4]}|${edge}`);
		return "stored";
	});
	const { video, source } = mount(real("X"), null);
	let css = 240;
	Object.defineProperty(video, "clientWidth", { configurable: true, get: () => css });
	video.load();
	css = 722;
	await missed(source);
	await missed(source);
	css = 240;
	source.setAttribute("src", real("Y"));
	css = 722;
	await missed(source);
	expect(source.getAttribute("src")).toContain("edge=1280");
	expect(calls).toEqual(["X|1280"]);
});
