// Paints decoded animated covers over the element TIDAL created. Rust sends raw I420 (12 bits a
// pixel against RGBA's 32) and a fragment shader converts it to RGB.

export interface CoverFrame {
	y: Uint8Array;
	u: Uint8Array;
	v: Uint8Array;
	yStride: number;
	uStride: number;
	vStride: number;
	ptsMicros: number;
}

export interface CoverAnimation {
	width: number;
	height: number;
	frames: CoverFrame[];
}

const MAGIC = 0x3143564c; // "LVC1" read as a little-endian u32
const HEADER_LEN = 16;
const RECORD_LEN = 32;

/**
 * Rust's `MAX_COVER_EDGE`: the size the still cover is requested at and the canvas's shape before
 * the first frame (the decoded size then replaces it). Must stay a size TIDAL serves stills in.
 */
const COVER_EDGE = 640;

/**
 * A still cover at `COVER_EDGE`, or null. It comes from TIDAL's media item or a legacy artwork's
 * `poster` and lands in a CSS `url()`, which is why it is validated; only a trailing
 * `/<w>x<h>.<ext>` is rewritten.
 */
export const stillCoverUrl = (raw: unknown): string | null => {
	if (typeof raw !== "string") return null;
	let parsed: URL;
	try {
		parsed = new URL(raw);
	} catch {
		return null;
	}
	if (parsed.protocol !== "https:" || parsed.hostname !== "resources.tidal.com") return null;
	if (!parsed.pathname.startsWith("/images/")) return null;
	parsed.pathname = parsed.pathname.replace(/\/\d+x\d+(\.[a-z]+)$/, `/${COVER_EDGE}x${COVER_EDGE}$1`);
	return parsed.href;
};

/**
 * The playing track's still, from its item (the animated artwork's path is unrelated).
 * Module-level: `load()` hands over only an element.
 */
let currentStill: string | null = null;

export const isCoverUrl = (url: string) => {
	if (typeof url !== "string") return false;
	try {
		const parsed = new URL(url);
		return (
			parsed.protocol === "https:" &&
			parsed.hostname === "resources.tidal.com" &&
			parsed.pathname.startsWith("/videos/")
		);
	} catch {
		return false;
	}
};

// Read the frames out of a body. Refuses rather than repairs: a body that is truncated or
// foreign would paint noise where the still cover used to be.
export const decodeCoverBuffer = (buf: ArrayBuffer): CoverAnimation => {
	if (buf.byteLength < HEADER_LEN) throw new Error("cover body is shorter than its header");
	const view = new DataView(buf);
	if (view.getUint32(0, true) !== MAGIC) throw new Error("cover body is not in the LVC1 format");

	const width = view.getUint32(4, true);
	const height = view.getUint32(8, true);
	const count = view.getUint32(12, true);

	const records = HEADER_LEN + count * RECORD_LEN;
	if (buf.byteLength < records) throw new Error("cover body is shorter than its frame records");

	const frames: CoverFrame[] = [];
	let at = records;
	for (let i = 0; i < count; i++) {
		const rec = HEADER_LEN + i * RECORD_LEN;
		const ptsMicros = Number(view.getBigInt64(rec, true));
		const yStride = view.getUint32(rec + 8, true);
		const uStride = view.getUint32(rec + 12, true);
		const vStride = view.getUint32(rec + 16, true);
		const yLen = view.getUint32(rec + 20, true);
		const uLen = view.getUint32(rec + 24, true);
		const vLen = view.getUint32(rec + 28, true);
		// A zero stride divides the plane's own width later, at paint time, into Infinity or NaN:
		// a malformed body belongs here, at the one place that already refuses one.
		if (yStride === 0 || uStride === 0 || vStride === 0) {
			throw new Error(`cover body has a zero stride in frame ${i}`);
		}
		if (at + yLen + uLen + vLen > buf.byteLength) {
			throw new Error(`cover body ends inside frame ${i}`);
		}
		frames.push({
			y: new Uint8Array(buf, at, yLen),
			u: new Uint8Array(buf, at + yLen, uLen),
			v: new Uint8Array(buf, at + yLen + uLen, vLen),
			yStride,
			uStride,
			vStride,
			ptsMicros,
		});
		at += yLen + uLen + vLen;
	}
	return { width, height, frames };
};

const VERTEX_SRC = `
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
	v_uv = vec2(a_pos.x * 0.5 + 0.5, 0.5 - a_pos.y * 0.5);
	gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

// BT.601 limited range. The horizontal scales crop the stride padding back out: each plane was
// uploaded at its full stride width, so sampling at the raw coordinate would read padding as
// picture on the right-hand edge.
const FRAGMENT_SRC = `
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
uniform float u_luma_scale;
uniform float u_chroma_scale;
void main() {
	float y = texture2D(u_y, vec2(v_uv.x * u_luma_scale, v_uv.y)).r;
	float u = texture2D(u_u, vec2(v_uv.x * u_chroma_scale, v_uv.y)).r - 0.5;
	float v = texture2D(u_v, vec2(v_uv.x * u_chroma_scale, v_uv.y)).r - 0.5;
	y = 1.1643 * (y - 0.0625);
	gl_FragColor = vec4(
		y + 1.5958 * v,
		y - 0.39173 * u - 0.81290 * v,
		y + 2.017 * u,
		1.0
	);
}`;

// The total loop duration, including a last frame whose own duration was never transmitted.
// Rust sends a presentation time per frame and nothing else: the timescale stays local to the
// demuxer, so a cadence exists only as the gap between consecutive frames. The median gap is
// what the last frame is held for.
const loopLength = (frames: CoverFrame[]) => {
	if (frames.length < 2) return 0;
	const deltas = frames.slice(1).map((f, i) => f.ptsMicros - frames[i].ptsMicros);
	deltas.sort((a, b) => a - b);
	const median = deltas[Math.floor(deltas.length / 2)];
	return frames[frames.length - 1].ptsMicros + median;
};

const compile = (gl: WebGLRenderingContext, type: number, src: string) => {
	const shader = gl.createShader(type)!;
	gl.shaderSource(shader, src);
	gl.compileShader(shader);
	if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
		throw new Error(`cover shader failed to compile: ${gl.getShaderInfoLog(shader)}`);
	}
	return shader;
};

const planeTexture = (gl: WebGLRenderingContext, unit: number) => {
	const tex = gl.createTexture()!;
	gl.activeTexture(gl.TEXTURE0 + unit);
	gl.bindTexture(gl.TEXTURE_2D, tex);
	gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
	gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
	gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
	gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
	return tex;
};

/**
 * The last decoded cover, for a later `load()` to paint without awaiting. One slot: an entry is
 * every frame uncompressed (see `MAX_COVER_EDGE`, src/video/cover.rs), and only the current
 * track's is painted.
 */
let coverCache: { src: string; anim: CoverAnimation } | null = null;

/**
 * TIDAL's artwork frame holds ONE in-flow artwork (`<img>` or `<video>`, `width:100%;
 * height:auto`) and takes its height from it; a second square would stack below. While we paint,
 * theirs is hidden by a rule, not an inline style: React replaces the now-playing artwork's failed
 * `<video>` with an `<img>`, and the rule matches whatever comes next. The rule's (0,1,1) beats
 * their single class without `!important`.
 */
const MARKER = "data-luna-cover";
const STYLE_ID = "luna-cover-invariant";
const ensureInvariantStyle = () => {
	if (document.getElementById(STYLE_ID)) return;
	const style = document.createElement("style");
	style.id = STYLE_ID;
	style.textContent = `[${MARKER}] > img, [${MARKER}] > video { display: none; }`;
	document.head.appendChild(style);
};

/**
 * Remove `canvas`, and release the frame only if it still owns it: a newer call for a later
 * track may already hold the slot, and a late older call must not disarm it.
 */
const detachCover = (surface: HTMLElement, canvas: HTMLCanvasElement) => {
	canvas.remove();
	if ((surface as any)._lunarCoverCanvas !== canvas) return;
	releaseSurface(surface);
};

/**
 * Give one frame back to TIDAL: our canvas goes, their artwork comes out of hiding.
 *
 * A draw loop whose canvas has left the document returns on its next tick and schedules nothing
 * more, so removing the canvas is the whole shutdown.
 */
const releaseSurface = (surface: HTMLElement) => {
	const held = (surface as any)._lunarCoverCanvas as HTMLCanvasElement | undefined;
	(surface as any)._lunarCoverCanvas = undefined;
	held?.remove();
	abandonCoverRequest(surface);
	(surface as any)._lunarCoverSrc = undefined;
	surface.removeAttribute(MARKER);
	nowPlayingSurfaces.delete(surface);
	forgetUnusedCover();
};

/**
 * Aborting is what reaches the Rust side's cancellation: it turns into `Cancel` on the resource
 * handler, which stops a download and decode nobody will read. Skipping through a queue starts
 * one per track.
 */
const abandonCoverRequest = (surface: HTMLElement) => {
	const pending = (surface as any)._lunarCoverAbort as AbortController | undefined;
	(surface as any)._lunarCoverAbort = undefined;
	pending?.abort();
};

/**
 * Drop the decoded cover once no frame paints it. The marker is the registry (an element
 * carries it exactly while one of our canvases is its artwork): a source no marked frame claims
 * has no reader left.
 */
const forgetUnusedCover = () => {
	const held = coverCache;
	if (!held) return;
	const stillPainted = Array.from(document.querySelectorAll(`[${MARKER}]`)).some(
		(surface) => (surface as any)._lunarCoverSrc === held.src,
	);
	if (!stillPainted) coverCache = null;
};

/**
 * The track now playing, as TIDAL announces it, for its still only. A frame's lifetime is never
 * the announce's: it can come before or after the artwork changes, and a next track from the same
 * album changes no artwork at all. The surface itself says when to let go (`evaluateSurface`).
 *
 * The now-playing artwork shows the same item the announce describes. Its frames take the new
 * still whenever it changes, in whichever order the two arrive. A new id resets the still; a
 * repeat of the same id (TIDAL announces a track several times per play) only fills one in. A new
 * id with no still empties the background, and `reconcileMarker` then decides what the frame is
 * worth without it.
 */
let currentTrack: string | null = null;
const nowPlayingSurfaces = new Set<HTMLElement>();
export const setCurrentTrack = (item: {
	id?: unknown;
	productId?: unknown;
	imageUrl?: unknown;
}) => {
	const id = String(item?.productId ?? item?.id ?? "");
	const still = stillCoverUrl(item?.imageUrl);
	if (id === currentTrack && (still === null || still === currentStill)) return;
	currentTrack = id;
	currentStill = still;

	for (const surface of nowPlayingSurfaces) {
		const canvas = (surface as any)._lunarCoverCanvas as HTMLCanvasElement | undefined;
		if (!canvas) continue;
		layStill(canvas, currentStill);
		reconcileMarker(surface);
	}
};

/**
 * The marker's one writer once a canvas is in place: it promises that our canvas shows something
 * in place of TIDAL's artwork, frames or a still, and is derived from that every time either
 * changes. A canvas with neither is not an artwork: while its decode is in flight (the abort
 * controller is held) it stays unmarked, as at attach, and a settled one (refused, failed) has
 * nothing left to wait for and hands the frame back.
 */
const reconcileMarker = (surface: HTMLElement) => {
	const canvas = (surface as any)._lunarCoverCanvas as HTMLCanvasElement | undefined;
	if (!canvas) return;
	if ((canvas as any)._lunarPainting || canvas.style.backgroundImage) {
		surface.setAttribute(MARKER, "");
	} else if ((surface as any)._lunarCoverAbort) {
		surface.removeAttribute(MARKER);
	} else {
		releaseSurface(surface);
	}
};

/**
 * The still goes UNDERNEATH, as a CSS background: the WebGL context rules out drawing an image
 * into the canvas, and the first decoded frame covers it with no swap. `cover` and `0 0` mirror
 * `_artworkImage_`'s `object-fit: cover; object-position: 0 0`.
 */
const layStill = (canvas: HTMLCanvasElement, still: string | null) => {
	canvas.style.backgroundImage = still ? `url("${still}")` : "";
	canvas.style.backgroundSize = still ? "cover" : "";
	canvas.style.backgroundPosition = still ? "0 0" : "";
};

/**
 * The still that belongs under this frame, told by the element rather than by attach order. TIDAL
 * paints animated covers through two components: the legacy one (album header, enlarge modal)
 * carries its own album's still as `poster`, and the now-playing artwork has none, its still being
 * the playing track's. Only the latter follows the announce.
 */
const stillFor = (el: HTMLMediaElement, surface: HTMLElement): string | null => {
	const poster = el.getAttribute("poster");
	if (poster) return stillCoverUrl(poster);
	nowPlayingSurfaces.add(surface);
	return currentStill;
};

/**
 * A resource selection on `el`, as its `loadstart` reports one: attached unless this very
 * selection already was. Every selection fires it, whatever started it: `load()`, a `src`, or a
 * `<source>` inserted into a fresh `<video>`, which is how TIDAL remounts the now-playing cover
 * after an error, with no `load()` to hook.
 */
export const coverSelected = (el: HTMLMediaElement) => {
	const src = el.currentSrc;
	if (el.localName !== "video" || !isCoverUrl(src)) return;
	if ((el as any)._lunarCoverAttached === src) return;
	attachCoverCanvas(el, src);
};

/**
 * A held frame's lifetime, read off the surface whenever it changes: the announce and `load()`
 * come in either order or not at all, while every change of artwork is a change here. What the
 * surface shows decides, never the order of the records:
 * - a `<video>` keeps the frame, and attaches its cover if this element was never handed over,
 *   in the commit's own microtask rather than at its `loadstart`, one task later;
 * - TIDAL's error fallback keeps it, being our failed `<video>`'s stand-in: always a `data:`
 *   image with no `srcset` (`imageFallback`), which a real still never is;
 * - any other `<img>` is a still of TIDAL's own, and takes the frame back;
 * - no artwork at all takes it back too: a swap of artwork is one commit; an empty surface is
 *   TIDAL showing nothing (a cover-less track, an entity not loaded yet), never a step between two.
 */
const evaluateSurface = (surface: HTMLElement) => {
	if (!(surface as any)._lunarCoverCanvas) return;
	const artwork = Array.from(surface.children).find((c) => c.localName === "video" || c.localName === "img");
	if (!artwork) {
		releaseSurface(surface);
		return;
	}
	if (artwork.localName === "video") {
		const media = artwork as HTMLMediaElement;
		const next = media.querySelector("source")?.src || media.src;
		if (isCoverUrl(next) && (media as any)._lunarCoverAttached !== next) attachCoverCanvas(media, next);
		return;
	}
	const src = artwork.getAttribute("src") ?? "";
	const fallback = (src === "" || src.startsWith("data:")) && !artwork.hasAttribute("srcset");
	if (!fallback) releaseSurface(surface);
};

/**
 * Watch a surface's artwork once it holds a canvas, through an observer scoped to it (the rest
 * of the page costs nothing). Its own removal is invisible from inside: `CoverTether` reports it.
 */
const watchedSurfaces = new WeakSet<HTMLElement>();
const watchSurface = (surface: HTMLElement) => {
	if (watchedSurfaces.has(surface)) return;
	watchedSurfaces.add(surface);
	new MutationObserver(() => evaluateSurface(surface)).observe(surface, {
		childList: true,
		subtree: true,
		attributes: true,
		attributeFilter: ["src", "srcset"],
	});
};

/**
 * Carried inside each held canvas (fallback content, never rendered), to learn that the canvas left
 * the document. The DOM's removal enqueues `disconnectedCallback` for every descendant of the
 * removed node and runs it before the removing call returns, whatever went (the canvas, its
 * surface, any ancestor) and with no frame to wait for. A hidden window that renders nothing
 * still aborts a decode nobody will paint.
 */
const TETHER = "luna-cover-tether";
class CoverTether extends HTMLElement {
	lost?: () => void;
	disconnectedCallback() {
		// A move runs its reactions once it is back in: still connected, nothing lost.
		if (!this.isConnected) this.lost?.();
	}
}
const ensureTether = () => {
	if (!customElements.get(TETHER)) customElements.define(TETHER, CoverTether);
};

/**
 * Fetch one cover's frames and paint them in place of the artwork element TIDAL placed. A non-200
 * answer is not an error: with no codec the resource handler refuses with a named reason, and
 * `giveUp` then keeps the still cover, or hands the frame back when there is none.
 */
export const attachCoverCanvas = async (el: HTMLMediaElement, src: string) => {
	// Keyed on the SURFACE (the artwork frame), not the element: TIDAL's cover `<video>` is gone
	// before a decode returns and `load()` never comes again, while its parent outlives it. Taken
	// from the element, never named: TIDAL's class names are hashed and change with each release.
	const surface = el.parentElement;
	if (!surface) {
		console.log("[cover] not painting:", "the cover element had no parent to paint into");
		return;
	}
	// The selection this element made is now ours, whichever signal brought it: `load()` and its
	// `loadstart` describe the same one, and our own canvas going in must not attach it again.
	(el as any)._lunarCoverAttached = src;

	// Skip only when this surface is ALREADY painting this cover. Matching on the src alone would
	// make the first attempt the only one, and that one cannot attach (it is still decoding while
	// TIDAL takes the element away).
	if ((surface as any)._lunarCoverSrc === src && (surface as any)._lunarCoverCanvas?.isConnected) {
		return;
	}
	(surface as any)._lunarCoverSrc = src;

	// Tear down the previous cover on entry, whatever happens below: the last track's animation
	// never outlives a change of src. A superseded call has its request aborted, and one that
	// resumes anyway no longer owns the slot: each of its exits leaves ours alone.
	const previous = (surface as any)._lunarCoverCanvas as HTMLCanvasElement | undefined;
	(surface as any)._lunarCoverCanvas = undefined;
	previous?.remove();
	abandonCoverRequest(surface);
	// Marker off until we have something to show: the still at once, else the first decoded frame
	// (`takeOverFrame`).
	surface.removeAttribute(MARKER);
	ensureInvariantStyle();

	// Decided once, here: every use below reads this local, never the module's.
	const still = stillFor(el, surface);

	// Attached now, empty, before any fetch: TIDAL drops its artwork before a decode returns, and
	// the frame, sized by its artwork, would collapse to a strip. An untouched canvas is
	// transparent; every failure path without a still removes it.
	const canvas = document.createElement("canvas");

	// The canvas BECOMES TIDAL's artwork: it takes the `<video>`'s own classes (`_artworkImage_`,
	// `_artwork_` size, round and crop it), minus `loading`, the fetch state. Copied, not named: the
	// hashed names turn over.
	canvas.className = el.className
		.split(/\s+/)
		.filter((c) => c && c !== "loading")
		.join(" ");

	// Square from the first moment. A canvas is a replaced element and takes its intrinsic ratio
	// from these attributes, so leaving them at the 300x150 default would hand TIDAL's CSS the
	// same wrong shape that collapsed the frame when their own element went unloaded. The real
	// decoded dimensions replace these once they are known.
	canvas.width = COVER_EDGE;
	canvas.height = COVER_EDGE;

	layStill(canvas, still);

	// No geometry of ours, and child order does not matter: their gloss overlay is `position:absolute;
	// z-index:1`, above in-flow content either way.
	surface.appendChild(canvas);
	(surface as any)._lunarCoverCanvas = canvas;
	ensureTether();
	const tether = document.createElement(TETHER) as CoverTether;
	tether.lost = () => {
		if ((surface as any)._lunarCoverCanvas === canvas) releaseSurface(surface);
	};
	canvas.appendChild(tether);
	watchSurface(surface);

	// TIDAL's artwork steps aside once ours shows something at least as good: now with a still,
	// else at the first decoded frame. Ownership is re-read here, where hiding happens: a released
	// call that still hid the artwork would leave the square blank until the next animated cover.
	const takeOverFrame = () => {
		if ((surface as any)._lunarCoverCanvas !== canvas) return;
		reconcileMarker(surface);
	};
	if (still) takeOverFrame();

	// Every exit below with no still in hand gives the square back rather than leaving an empty one
	// where the artwork belongs: with our canvas gone the frame falls back to whatever TIDAL makes
	// of it.
	const giveUp = (why: string) => {
		// With a still, the canvas STAYS and TIDAL's artwork stays hidden. On the now-playing frame
		// TIDAL holds `imageFallback(...)`, the near-blank placeholder `artwork_filter` exists to
		// avoid; on a legacy frame its `<video>` shows its poster, the same still at a smaller
		// size. Every path here (no codec yet, refused body, network, no WebGL) is better served by
		// the still. Read off the canvas, not the attach's own: a now-playing frame takes the
		// announce's still whenever it arrives.
		if (canvas.style.backgroundImage && canvas.isConnected) {
			console.log("[cover] showing the still cover instead:", why);
			// The src is forgotten anyway, letting a later attempt at this same cover run instead of
			// being skipped as "already painting": the codec may have finished installing in between.
			if ((surface as any)._lunarCoverCanvas === canvas) {
				(surface as any)._lunarCoverSrc = undefined;
			}
			return;
		}
		console.log("[cover] not painting:", why);
		detachCover(surface, canvas);
	};

	let anim: CoverAnimation;
	const cached = coverCache?.src === src ? coverCache.anim : null;
	if (cached) {
		// From here to the first `requestAnimationFrame`, everything runs in the same task as the
		// `load()` that handed us the element: nothing can move underneath it.
		anim = cached;
	} else {
		// Carried on the surface so whoever gives this frame up can reach the request too: the
		// canvas is removable from several places, and the fetch has to go with it.
		const abort = new AbortController();
		(surface as any)._lunarCoverAbort = abort;

		// Given up only by the request that still holds the slot. A later call for this surface
		// replaces the controller with its own while this one is awaiting, and clearing the slot
		// blind would then leave THAT request with nothing able to stop it.
		const releaseAbort = () => {
			if ((surface as any)._lunarCoverAbort === abort) {
				(surface as any)._lunarCoverAbort = undefined;
			}
		};

		let res: Response;
		try {
			res = await fetch(`luna-cover://frames/?src=${encodeURIComponent(src)}`, {
				signal: abort.signal,
			});
		} catch (e) {
			releaseAbort();
			// An abort is our own doing (the frame was given back, or a newer call took it), not a
			// transport failure.
			if (!abort.signal.aborted) giveUp(`could not reach the frame transport: ${e}`);
			return;
		}
		if (!res.ok) {
			releaseAbort();
			giveUp(res.headers.get("X-Luna-Cover-Reason") ?? `status ${res.status}`);
			return;
		}

		// Held across the body: `fetch` settles on the headers, the frames arrive during `arrayBuffer()`,
		// and a track change in between must still find a controller to abort.
		let body: ArrayBuffer;
		try {
			body = await res.arrayBuffer();
		} catch (e) {
			releaseAbort();
			if (!abort.signal.aborted) giveUp(`could not read the frame body: ${e}`);
			return;
		}
		releaseAbort();

		try {
			anim = decodeCoverBuffer(body);
		} catch (e) {
			giveUp(`refused the body: ${e}`);
			return;
		}
		if (anim.frames.length === 0) {
			giveUp("decoded body has zero frames");
			return;
		}
		// The `<video>` is expected to be gone by now, and that is fine: it is not what we paint
		// into. The surface is. If even the frame went, there is nothing to hold a canvas.
		if (!surface.isConnected) {
			giveUp("the artwork surface left the document");
			return;
		}
	}

	// Staleness is ownership: a later call replaces this slot at entry, and a release empties it. A
	// canvas no longer in the slot has lost its frame.
	if ((surface as any)._lunarCoverCanvas !== canvas) {
		detachCover(surface, canvas);
		return;
	}

	// Cached only now, AFTER that check: written before it, a call that lost the race would evict
	// the cover actually painting. The slot only ever holds what a frame is showing, which is also
	// what makes it safe to drop when no frame shows it.
	coverCache = { src, anim };

	// The backing store, now that the decoded size is known, and exactly that size: it is the
	// canvas's intrinsic size, which gives `height:auto` its ratio, and a device-pixel-ratio term
	// would only invent pixels the decode never produced.
	canvas.width = anim.width;
	canvas.height = anim.height;

	// The success counterpart of the `[cover] not painting:` lines above, telling "never ran" from
	// "ran and painted nowhere". The box is the one number that says the frame took our square.
	const box = canvas.getBoundingClientRect();
	console.log(
		`[cover] painting ${anim.frames.length} frames of ${anim.width}x${anim.height}` +
			` in a ${Math.round(box.width)}x${Math.round(box.height)} frame`,
	);

	const gl = canvas.getContext("webgl");
	if (!gl) {
		giveUp("could not acquire a WebGL context");
		return;
	}

	// Setup can throw (shader compile or link). The call is fire-and-forget: an escape would be an
	// unhandled rejection, orphaning the appended canvas over TIDAL's element.
	try {
		const program = gl.createProgram()!;
		gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX_SRC));
		gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT_SRC));
		gl.linkProgram(program);
		gl.useProgram(program);

		const quad = gl.createBuffer();
		gl.bindBuffer(gl.ARRAY_BUFFER, quad);
		gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
		const aPos = gl.getAttribLocation(program, "a_pos");
		gl.enableVertexAttribArray(aPos);
		gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);

		const textures = [planeTexture(gl, 0), planeTexture(gl, 1), planeTexture(gl, 2)];
		gl.uniform1i(gl.getUniformLocation(program, "u_y"), 0);
		gl.uniform1i(gl.getUniformLocation(program, "u_u"), 1);
		gl.uniform1i(gl.getUniformLocation(program, "u_v"), 2);
		const lumaScaleAt = gl.getUniformLocation(program, "u_luma_scale");
		const chromaScaleAt = gl.getUniformLocation(program, "u_chroma_scale");
		gl.viewport(0, 0, canvas.width, canvas.height);

		// Unless the still cover already did, the frame changes hands here: everything that could
		// fail is behind us.
		(canvas as any)._lunarPainting = true;
		takeOverFrame();

		const chromaWidth = Math.ceil(anim.width / 2);
		const chromaRows = Math.ceil(anim.height / 2);
		const total = loopLength(anim.frames);
		const started = performance.now();
		let shown = -1;

		const draw = () => {
			// Liveness follows the SURFACE, deliberately not TIDAL's `<video>`: that element is almost
			// always gone before the first frame is painted, and ending the loop with it would end
			// every loop.
			if (!surface.isConnected) {
				detachCover(surface, canvas);
				return;
			}
			if (!canvas.isConnected) return;

			// No resizing here. With buffer and box both square, `object-fit` has no letterboxing
			// left to defeat, and measuring a box whose height derives from the buffer's aspect ratio
			// is a loop feeding itself.
			const elapsed = (performance.now() - started) * 1000;
			const at = total > 0 ? elapsed % total : 0;
			let index = 0;
			for (let i = 0; i < anim.frames.length; i++) {
				if (anim.frames[i].ptsMicros <= at) index = i;
				else break;
			}

			// Uploading is what a new frame costs, so that alone is gated on the index moving.
			if (index !== shown) {
				shown = index;
				const f = anim.frames[index];
				gl.uniform1f(lumaScaleAt, anim.width / f.yStride);
				gl.uniform1f(chromaScaleAt, chromaWidth / f.uStride);
				const planes: Array<[Uint8Array, number, number]> = [
					[f.y, f.yStride, anim.height],
					[f.u, f.uStride, chromaRows],
					[f.v, f.vStride, chromaRows],
				];
				gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
				planes.forEach(([data, stride, rows], unit) => {
					gl.activeTexture(gl.TEXTURE0 + unit);
					gl.bindTexture(gl.TEXTURE_2D, textures[unit]);
					gl.texImage2D(
						gl.TEXTURE_2D, 0, gl.LUMINANCE, stride, rows, 0,
						gl.LUMINANCE, gl.UNSIGNED_BYTE, data,
					);
				});
			}

			// Drawn on EVERY tick: without `preserveDrawingBuffer` (a copy per frame) the buffer is
			// undefined after compositing, and a tick that draws nothing shows tile-seamed garbage.
			gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
			requestAnimationFrame(draw);
		};
		requestAnimationFrame(draw);
	} catch (e) {
		giveUp(`failed to set up the shader program: ${e}`);
	}
};
