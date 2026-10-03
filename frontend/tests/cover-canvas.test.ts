// Tests `src/video/cover-canvas.ts`.
//
// The wire format is the contract between the Rust resource handler and this module: a header
// naming the format, then one record per frame, then the planes.

import { afterEach, expect, test } from "bun:test";

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

test("a buffer whose magic is not ours is refused", () => {
	const buf = buildBuffer(1, 4, 4);
	new Uint8Array(buf)[0] = 0x58;
	expect(() => decodeCoverBuffer(buf)).toThrow();
});

test("a buffer cut short of its planes is refused rather than half-painted", () => {
	const full = buildBuffer(2, 4, 4);
	expect(() => decodeCoverBuffer(full.slice(0, full.byteLength - 8))).toThrow();
});

test("a buffer whose stride is zero is refused rather than dividing by it at paint time", () => {
	const buf = buildBuffer(1, 4, 4);
	new DataView(buf).setUint32(16 + 8, 0, true); // the first frame's y_stride
	expect(() => decodeCoverBuffer(buf)).toThrow();
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

// Enough turns for the module to cross its `fetch` and settle on the body it is now awaiting.
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

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
	const { surface, video } = mountFrame();

	setCurrentTrack({ id: "held-open-a" });
	const painting = attachCoverCanvas(video, coverSrc("held-open"));
	await settle();

	// The response headers are in, the frames are not: the span a controller released at the
	// headers would leave uncancellable.
	expect(transport.signal).toBeDefined();
	expect(transport.signal!.aborted).toBe(false);

	setCurrentTrack({ id: "held-open-b" });
	expect(transport.signal!.aborted).toBe(true);

	transport.deliver(buildBuffer(1, 4, 4));
	await painting;
	expect(surface.hasAttribute("data-luna-cover")).toBe(false);
});

// The transport stub above ignores the abort deliberately. Under test here is the staleness gate
// on its own: a body that arrives anyway, for a frame already handed back.
test("a cover decoded after its frame was given back leaves TIDAL's artwork alone", async () => {
	stubWebgl();
	const transport = transportHeldOpen();
	const { surface, video } = mountFrame();

	setCurrentTrack({ id: "given-back-a" });
	const painting = attachCoverCanvas(video, coverSrc("given-back"));
	await settle();

	setCurrentTrack({ id: "given-back-b" });
	transport.deliver(buildBuffer(1, 4, 4));
	await painting;

	// Hiding their artwork with nothing of ours in the frame is the blank square: the marker hides
	// it until a release or the next attach takes it off.
	expect(surface.hasAttribute("data-luna-cover")).toBe(false);
	expect(surface.querySelector("canvas")).toBeNull();
});

// The still is the floor under every failure (no codec yet, a refused body), and a retry must
// not take it away.
test("a retry for the same cover keeps the still the frame is entitled to", async () => {
	stubWebgl();
	(globalThis as any).fetch = () =>
		Promise.resolve({
			ok: false,
			status: 503,
			headers: { get: () => "the codec is still installing" },
			arrayBuffer: () => Promise.reject(new Error("a refused body has none")),
		});
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

// The other half of the same rule. Identity grants the still: a frame that is not the playing
// track's gets nothing even while that track's still is sitting in the module.
test("a frame that is not the playing track's never inherits its still", async () => {
	stubWebgl();
	(globalThis as any).fetch = () =>
		Promise.resolve({
			ok: false,
			status: 503,
			headers: { get: () => "the codec is still installing" },
			arrayBuffer: () => Promise.reject(new Error("a refused body has none")),
		});
	const player = mountFrame();
	const header = mountFrame();
	setCurrentTrack({
		id: "not-mine",
		imageUrl: "https://resources.tidal.com/images/a/b/1280x1280.jpg",
	});

	await attachCoverCanvas(player.video, coverSrc("not-mine-player"));
	await attachCoverCanvas(header.video, coverSrc("not-mine-header"));

	expect(player.surface.hasAttribute("data-luna-cover")).toBe(true);
	expect(header.surface.querySelector("canvas")).toBeNull();
	expect(header.surface.hasAttribute("data-luna-cover")).toBe(false);
});

test("a cover whose frame is still its own takes the artwork over", async () => {
	stubWebgl();
	const transport = transportHeldOpen();
	const { surface, video } = mountFrame();

	setCurrentTrack({ id: "still-ours" });
	const painting = attachCoverCanvas(video, coverSrc("still-ours"));
	await settle();
	transport.deliver(buildBuffer(1, 4, 4));
	await painting;

	expect(surface.hasAttribute("data-luna-cover")).toBe(true);
	expect(surface.querySelector("canvas")).not.toBeNull();
});
