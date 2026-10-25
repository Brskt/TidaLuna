// Tests `src/video/cover-canvas.ts`.
//
// The wire format is the contract between the Rust resource handler and this module: a header
// naming the format, then one record per frame, then the planes.

import { afterEach, expect, test } from "bun:test";

import { refusingTransport } from "./helpers/refusing-transport";
import {
	attachCoverCanvas,
	decodeCoverBuffer,
	isCoverUrl,
	setCurrentTrack,
	stillCoverUrl,
} from "../src/video/cover-canvas";

const buildBuffer = (frames: number, width: number, height: number) => {
	const recordLen = 32;
	const ySize = width * height;
	const cSize = (width / 2) * (height / 2);
	const total = 16 + frames * recordLen + frames * (ySize + 2 * cSize);
	const buf = new ArrayBuffer(total);
	const view = new DataView(buf);
	const bytes = new Uint8Array(buf);
	bytes.set([0x4c, 0x56, 0x43, 0x31], 0); // "LVC1"
	view.setUint32(4, width, true);
	view.setUint32(8, height, true);
	view.setUint32(12, frames, true);
	for (let i = 0; i < frames; i++) {
		const at = 16 + i * recordLen;
		view.setBigInt64(at, BigInt(i * 41_666), true); // 24 fps, µs
		view.setUint32(at + 8, width, true);
		view.setUint32(at + 12, width / 2, true);
		view.setUint32(at + 16, width / 2, true);
		view.setUint32(at + 20, ySize, true);
		view.setUint32(at + 24, cSize, true);
		view.setUint32(at + 28, cSize, true);
	}
	return buf;
};

test("a well-formed buffer decodes to its frames, in order", () => {
	const anim = decodeCoverBuffer(buildBuffer(3, 4, 4));
	expect(anim.width).toBe(4);
	expect(anim.height).toBe(4);
	expect(anim.frames.length).toBe(3);
	expect(anim.frames[1].ptsMicros).toBe(41_666);
	expect(anim.frames[0].y.length).toBe(16);
});

test("a cover url is recognised only on the artwork host, under the video path", () => {
	expect(isCoverUrl("https://resources.tidal.com/videos/a/1280x1280.mp4")).toBe(true);
	expect(isCoverUrl("https://resources.tidal.com/images/a/1280x1280.jpg")).toBe(false);
	expect(isCoverUrl("https://evil.example/videos/a.mp4")).toBe(false);
	expect(isCoverUrl("http://resources.tidal.com/videos/a/1280x1280.mp4")).toBe(false);
});

// The still cover is shown under the animation while it decodes, and it reaches this module from
// TIDAL's own media item: a foreign string that ends up inside a CSS `url()`.
test("a still cover is taken only from the artwork host, resized to the canvas", () => {
	expect(stillCoverUrl("https://resources.tidal.com/images/a/b/1280x1280.jpg")).toBe(
		"https://resources.tidal.com/images/a/b/640x640.jpg",
	);
	// A path carrying no size is left as it is rather than guessed at.
	expect(stillCoverUrl("https://resources.tidal.com/images/a/b/cover.jpg")).toBe(
		"https://resources.tidal.com/images/a/b/cover.jpg",
	);
	expect(stillCoverUrl("https://evil.example/images/a/1280x1280.jpg")).toBeNull();
	expect(stillCoverUrl("http://resources.tidal.com/images/a/1280x1280.jpg")).toBeNull();
	expect(stillCoverUrl("https://resources.tidal.com/videos/a/1280x1280.mp4")).toBeNull();
	expect(stillCoverUrl("javascript:alert(1)")).toBeNull();
	expect(stillCoverUrl(undefined)).toBeNull();
});

// The playing track can change while a cover is still arriving. Everything below is about that
// overlap: who may still hide TIDAL's artwork, and what has to stay cancellable for how long.

// One per test, never shared. `coverCache` is a module-level slot that survives between tests, and
// a second test reusing a source another already decoded takes the cached path instead, which is
// fully synchronous and never reaches the overlap these tests exist to exercise.
const coverSrc = (name: string) => `https://resources.tidal.com/videos/${name}/640x640.mp4`;

const realFetch = globalThis.fetch;
const realRaf = globalThis.requestAnimationFrame;
const realGetContext = (globalThis as any).HTMLCanvasElement?.prototype?.getContext;

// happy-dom has no WebGL, and without a context the module gives up before the paint path. The
// stub answers every call that path makes: one it lacked would throw into the module's own catch
// and read as a give-up, not a failure.
const stubWebgl = () => {
	const handle = () => ({});
	const gl: Record<string, unknown> = {
		VERTEX_SHADER: 1,
		FRAGMENT_SHADER: 2,
		COMPILE_STATUS: 3,
		ARRAY_BUFFER: 4,
		STATIC_DRAW: 5,
		TEXTURE_2D: 6,
		TEXTURE0: 7,
		LUMINANCE: 8,
		UNSIGNED_BYTE: 9,
		TRIANGLE_STRIP: 10,
		FLOAT: 11,
		UNPACK_ALIGNMENT: 12,
		TEXTURE_MIN_FILTER: 13,
		TEXTURE_MAG_FILTER: 14,
		TEXTURE_WRAP_S: 15,
		TEXTURE_WRAP_T: 16,
		LINEAR: 17,
		CLAMP_TO_EDGE: 18,
		createProgram: handle,
		createShader: handle,
		createBuffer: handle,
		createTexture: handle,
		getUniformLocation: handle,
		getShaderParameter: () => true,
		getAttribLocation: () => 0,
		shaderSource: () => {},
		compileShader: () => {},
		attachShader: () => {},
		linkProgram: () => {},
		useProgram: () => {},
		bindBuffer: () => {},
		bufferData: () => {},
		enableVertexAttribArray: () => {},
		vertexAttribPointer: () => {},
		activeTexture: () => {},
		bindTexture: () => {},
		texParameteri: () => {},
		texImage2D: () => {},
		uniform1i: () => {},
		uniform1f: () => {},
		viewport: () => {},
		pixelStorei: () => {},
		drawArrays: () => {},
	};
	(globalThis as any).HTMLCanvasElement.prototype.getContext = () => gl;
	// The draw loop is not what is under test here, and letting it schedule itself would keep
	// running between tests.
	(globalThis as any).requestAnimationFrame = () => 0;
};

// A frame as TIDAL builds one: an artwork element inside the box that holds it.
const mountFrame = () => {
	const surface = document.createElement("div");
	const video = document.createElement("video");
	surface.appendChild(video);
	document.body.appendChild(surface);
	return { surface, video };
};

// A transport whose body is handed over on demand, so a test can hold a cover mid-flight and act
// while it is there.
const transportHeldOpen = () => {
	let deliver: (body: ArrayBuffer) => void = () => {};
	const body = new Promise<ArrayBuffer>((resolve) => {
		deliver = resolve;
	});
	let signal: AbortSignal | undefined;
	(globalThis as any).fetch = (_url: string, init: { signal: AbortSignal }) => {
		signal = init.signal;
		return Promise.resolve({
			ok: true,
			headers: { get: () => null },
			arrayBuffer: () => body,
		});
	};
	return {
		deliver,
		// Read lazily: the fetch has not been issued when this object is built.
		get signal() {
			return signal;
		},
	};
};

// Enough turns for the module to cross its `fetch` and settle on the body it is now awaiting, and
// for the surface observer to see a change.
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

// TIDAL swaps its artwork element as React does: the new one takes the old one's place.
const replaceArtwork = (frame: { surface: HTMLElement }, next: Element) => {
	const current = Array.from(frame.surface.children).find((c) => c.localName === "video" || c.localName === "img");
	current!.replaceWith(next);
	return next;
};

// The artwork TIDAL shows when it takes the frame back: a still of its own, with its `srcset`.
const tidalStill = () => {
	const img = document.createElement("img");
	img.setAttribute("src", "https://resources.tidal.com/images/t/t/1280x1280.jpg");
	img.setAttribute("srcset", "https://resources.tidal.com/images/t/t/1280x1280.jpg 1280w");
	return img;
};

// What TIDAL renders when its cover `<video>` fails: `imageFallback(...)`, a `data:` image.
const tidalFallback = () => {
	const img = document.createElement("img");
	img.setAttribute("src", "data:image/svg+xml;base64,PHN2Zy8+");
	return img;
};

// A cover `<video>` as TIDAL mounts one, its cover on a `<source>` child.
const coverVideo = (src: string) => {
	const video = document.createElement("video");
	const source = document.createElement("source");
	source.setAttribute("src", src);
	video.appendChild(source);
	return video;
};

afterEach(() => {
	globalThis.fetch = realFetch;
	globalThis.requestAnimationFrame = realRaf;
	if (realGetContext) {
		(globalThis as any).HTMLCanvasElement.prototype.getContext = realGetContext;
	}
	document.body.replaceChildren();
});

test("a cover stays cancellable while its body is still arriving", async () => {
	stubWebgl();
	const transport = transportHeldOpen();
	const frame = mountFrame();
	const { surface, video } = frame;

	setCurrentTrack({ id: "held-open-a" });
	const painting = attachCoverCanvas(video, coverSrc("held-open"));
	await settle();

	// The response headers are in, the frames are not: the span a controller released at the
	// headers would leave uncancellable.
	expect(transport.signal).toBeDefined();
	expect(transport.signal!.aborted).toBe(false);

	replaceArtwork(frame, tidalStill());
	await settle();
	expect(transport.signal!.aborted).toBe(true);

	transport.deliver(buildBuffer(1, 4, 4));
	await painting;
	expect(surface.hasAttribute("data-luna-cover")).toBe(false);
});

// The decode cache holds what a frame is showing, and a frame given back shows nothing. A late
// body that went into it anyway would sit there with no reader, and stand in for a fetch the
// next frame of the same cover is owed.
test("a cover decoded after its frame was given back is not kept for the next frame", async () => {
	stubWebgl();
	const transport = transportHeldOpen();
	const frame = mountFrame();

	setCurrentTrack({ id: "not-kept-a" });
	const painting = attachCoverCanvas(frame.video, coverSrc("not-kept"));
	await settle();

	replaceArtwork(frame, tidalStill());
	await settle();
	transport.deliver(buildBuffer(1, 4, 4));
	await painting;

	let fetched = 0;
	(globalThis as any).fetch = () => {
		fetched++;
		return Promise.resolve({ ok: true, headers: { get: () => null }, arrayBuffer: () => Promise.resolve(buildBuffer(1, 4, 4)) });
	};
	await attachCoverCanvas(mountFrame().video, coverSrc("not-kept"));

	expect(fetched).toBe(1);
});

// The still is the floor under every failure (no codec yet, a refused body), and a retry must
// not take it away.
test("a retry for the same cover keeps the still the frame is entitled to", async () => {
	stubWebgl();
	refusingTransport();
	const { surface, video } = mountFrame();
	setCurrentTrack({
		id: "retry",
		imageUrl: "https://resources.tidal.com/images/a/b/1280x1280.jpg",
	});

	await attachCoverCanvas(video, coverSrc("retry"));
	const first = surface.querySelector("canvas");
	expect(first).not.toBeNull();
	expect((first as HTMLCanvasElement).style.backgroundImage).toContain("640x640.jpg");
	expect(surface.hasAttribute("data-luna-cover")).toBe(true);

	// TIDAL mounts its cover element again for the same track.
	await attachCoverCanvas(video, coverSrc("retry"));
	const second = surface.querySelector("canvas");
	expect(second).not.toBeNull();
	expect((second as HTMLCanvasElement).style.backgroundImage).toContain("640x640.jpg");
	expect(surface.hasAttribute("data-luna-cover")).toBe(true);
});

// Every cover decodes at once: these tests are about which frame is released, not about timing.
const transportDecoding = () => {
	(globalThis as any).fetch = () =>
		Promise.resolve({ ok: true, headers: { get: () => null }, arrayBuffer: () => Promise.resolve(buildBuffer(2, 4, 4)) });
};

// TIDAL's legacy artwork (album header, enlarge modal): the `<video>` names its album's still.
const mountLegacyFrame = (album: string) => {
	const frame = mountFrame();
	frame.video.setAttribute("poster", `https://resources.tidal.com/images/${album}/${album}/160x160.jpg`);
	return frame;
};

const playing = (id: string, album: string) =>
	setCurrentTrack({ id, imageUrl: `https://resources.tidal.com/images/${album}/${album}/1280x1280.jpg` });

// A track change releases no frame: the announce can land before or after the artwork changes,
// and only the surface knows which. The now-playing frame takes the new still, an album header
// keeps its own.
test("a track change gives the now-playing frame its still and leaves a legacy frame alone", async () => {
	stubWebgl();
	transportDecoding();
	const header = mountLegacyFrame("y");
	const player = mountFrame();
	playing("release-1", "x");

	await attachCoverCanvas(header.video, coverSrc("release-header"));
	await attachCoverCanvas(player.video, coverSrc("release-player"));
	const headerCanvas = header.surface.querySelector("canvas") as HTMLCanvasElement;
	const playerCanvas = player.surface.querySelector("canvas") as HTMLCanvasElement;

	playing("release-2", "z");

	expect(headerCanvas.isConnected).toBe(true);
	expect(headerCanvas.style.backgroundImage).toContain("/y/y/640x640.jpg");
	expect(playerCanvas.isConnected).toBe(true);
	expect(playerCanvas.style.backgroundImage).toContain("/z/z/640x640.jpg");
	expect(player.surface.hasAttribute("data-luna-cover")).toBe(true);
});

// The draw loop runs per frame; these tests turn it by hand.
const manualFrames = () => {
	let queued: FrameRequestCallback[] = [];
	(globalThis as any).requestAnimationFrame = (cb: FrameRequestCallback) => {
		queued.push(cb);
		return queued.length;
	};
	return () => {
		const due = queued;
		queued = [];
		for (const cb of due) cb(0);
	};
};

// TIDAL remounts the now-playing artwork within one track (view closed and reopened, desktop and
// mobile layouts swapping): the new frame gets the still too, and the one it replaced is let go,
// its request with it.
test("a remounted now-playing frame takes the still, the frame it replaced is let go", async () => {
	stubWebgl();
	const nextFrame = manualFrames();
	const transport = transportHeldOpen();
	playing("remount-1", "x");
	const first = mountFrame();
	const firstPainting = attachCoverCanvas(first.video, coverSrc("remount-first"));
	await settle();
	first.surface.remove();
	nextFrame();
	expect(transport.signal!.aborted).toBe(true);
	expect(first.surface.querySelector("canvas")).toBeNull();
	transport.deliver(buildBuffer(1, 4, 4));
	await firstPainting;

	transportDecoding();
	const second = mountFrame();
	await attachCoverCanvas(second.video, coverSrc("remount-second"));
	expect((second.surface.querySelector("canvas") as HTMLCanvasElement).style.backgroundImage).toContain("/x/x/640x640.jpg");

	playing("remount-2", "z");
	expect((second.surface.querySelector("canvas") as HTMLCanvasElement).style.backgroundImage).toContain("/z/z/640x640.jpg");
});

// When the cover `<video>` fails, TIDAL renders its placeholder in its place: that is our failed
// element's stand-in, not a still of TIDAL's to give the frame back to.
test("TIDAL's error placeholder does not take the frame back, its own still does", async () => {
	stubWebgl();
	transportDecoding();
	const frame = mountFrame();
	playing("fallback", "f");
	await attachCoverCanvas(frame.video, coverSrc("fallback"));

	replaceArtwork(frame, tidalFallback());
	await settle();
	expect(frame.surface.querySelector("canvas")).not.toBeNull();
	expect(frame.surface.hasAttribute("data-luna-cover")).toBe(true);

	replaceArtwork(frame, tidalStill());
	await settle();
	expect(frame.surface.querySelector("canvas")).toBeNull();
	expect(frame.surface.hasAttribute("data-luna-cover")).toBe(false);
});

// After an error TIDAL mounts the next cover's `<video>` fresh and never calls `load()` on it:
// the `<source>` going in starts the selection by itself.
test("a fresh cover video mounted in a held frame is attached without a load()", async () => {
	stubWebgl();
	const requested: string[] = [];
	(globalThis as any).fetch = (url: string) => {
		requested.push(decodeURIComponent(url));
		return Promise.resolve({ ok: true, headers: { get: () => null }, arrayBuffer: () => Promise.resolve(buildBuffer(2, 4, 4)) });
	};
	const frame = mountFrame();
	playing("fresh-1", "a");
	await attachCoverCanvas(frame.video, coverSrc("fresh-a"));
	replaceArtwork(frame, tidalFallback());
	await settle();

	replaceArtwork(frame, coverVideo(coverSrc("fresh-b")));
	await settle();

	expect(requested.some((url) => url.includes(coverSrc("fresh-b")))).toBe(true);
	expect(frame.surface.querySelectorAll("canvas").length).toBe(1);
	expect(frame.surface.hasAttribute("data-luna-cover")).toBe(true);
});

// A legacy frame keeps its failed `<video>` (TIDAL gives it no error handler). Every later change
// of that surface finds the same element there: a refused cover must not be asked for again because
// of one.
test("a refused cover is not asked again when its frame changes", async () => {
	stubWebgl();
	let requests = 0;
	refusingTransport(() => requests++);
	const header = mountFrame();
	const video = coverVideo(coverSrc("refused-header"));
	video.setAttribute("poster", "https://resources.tidal.com/images/r/r/160x160.jpg");
	header.video.replaceWith(video);
	await attachCoverCanvas(video, coverSrc("refused-header"));
	header.surface.appendChild(document.createElement("div"));
	await settle();

	expect(requests).toBe(1);
	expect(header.surface.hasAttribute("data-luna-cover")).toBe(true);
});

// The marker hides TIDAL's artwork and promises that our canvas shows something in its place:
// decoded frames, or a still under a canvas whose decode has not landed.
test("a track without a still leaves the frame's decode running, unmarked until frames land", async () => {
	stubWebgl();
	const transport = transportHeldOpen();
	const { surface, video } = mountFrame();
	playing("bare-pending-1", "a");
	const painting = attachCoverCanvas(video, coverSrc("bare-pending"));
	await settle();
	expect(surface.hasAttribute("data-luna-cover")).toBe(true);

	setCurrentTrack({ id: "bare-pending-2" });
	const canvas = surface.querySelector("canvas") as HTMLCanvasElement;
	expect(canvas).not.toBeNull();
	expect(canvas.style.backgroundImage).toBe("");
	expect(surface.hasAttribute("data-luna-cover")).toBe(false);
	expect(transport.signal!.aborted).toBe(false);

	transport.deliver(buildBuffer(1, 4, 4));
	await painting;
	expect(canvas.isConnected).toBe(true);
	expect(surface.hasAttribute("data-luna-cover")).toBe(true);
});
