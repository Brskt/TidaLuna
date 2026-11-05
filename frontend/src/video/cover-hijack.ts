// Points TIDAL's own animated-cover `<video>` at a VP9 transcode we serve, instead of the H.264
// file this Chromium cannot play.
//
// `audio-proxy.ts` hands every write of the cover `<source>`'s `src` (attribute or property) to
// `hijackSourceSrc`, and every `load()` of its `<video>` to `retargetCover`. TIDAL keeps the rest:
// element, layout, `poster` (shown until the transcode plays), replay count.
//
// The first time a cover is seen it is not transcoded: `video/` answers 404, the element errors,
// `early-runtime/cover_error.js` keeps the error from TIDAL and tells us (`luna-cover-miss`), and
// the cover is transcoded once (`cover-encode.ts`), then every element showing it reloads.

import { transcodeCover, type TranscodeOutcome } from "./cover-encode";

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

const MAX_EDGE = 1280;
/** The size of an element that names none. */
const DEFAULT_EDGE = 640;

/** The sizes TIDAL publishes; Rust transcodes the smallest that covers the edge asked for. */
const SERVED_EDGES = [80, 160, 320, 640, 1280];
const servedEdge = (edge: number) => SERVED_EDGES.find((e) => e >= edge) ?? MAX_EDGE;

/**
 * The size to transcode at: the `width` TIDAL gave the `<video>` (1280 in the playback view,
 * none in the album header: 640), or its layout box in device pixels when that needs more; a box
 * of 0 (not laid out, detached) counts for nothing. Rounded up to a published size.
 */
export const coverEdge = (video: Element | null, real?: string) => {
	const width = Number(video?.getAttribute("width"));
	const base = Number.isInteger(width) && width > 0 ? Math.min(width, MAX_EDGE) : DEFAULT_EDGE;
	const box = video instanceof HTMLElement ? Math.max(video.clientWidth, video.clientHeight) : 0;
	let edge = Math.min(servedEdge(Math.max(base, box * window.devicePixelRatio)), MAX_EDGE);
	// A size whose transcode failed twice is not asked again while a smaller one, down to the file
	// the element would have had, can play.
	while (real && edge > servedEdge(base) && closed(coverKey(real, edge))) {
		edge = SERVED_EDGES.filter((e) => e < edge).pop() ?? servedEdge(base);
	}
	return edge;
};

/** One cover at one size, as Rust keys it: neither the variant TIDAL named nor an edge between
 * two published sizes makes another file. */
const coverKey = (real: string, edge: number) =>
	`${real.replace(/[?#].*/s, "").replace(/\/[^/]*$/, "")}|${servedEdge(edge)}`;

/**
 * The URL suffix per cover after a failure. The media stack never requests a `luna-cover://` URL
 * again once it failed, even marked `no-store`: every URL written after a miss, or after the
 * transcode was stored, must differ from the one that failed.
 */
const busts = new Map<string, number>();
let lastBust = 0;
/** Strictly increasing: two busts within one millisecond must still differ. */
const nextBust = () => (lastBust = Math.max(Date.now(), lastBust + 1));

export const lunaCoverUrl = (real: string, edge: number) => {
	const bust = busts.get(coverKey(real, edge));
	return `luna-cover://video/?src=${encodeURIComponent(real)}&edge=${edge}${bust ? `&r=${bust}` : ""}`;
};

interface Hijacked {
	real: string;
	edge: number;
}

type CoverSource = HTMLSourceElement & { _lunaCover?: Hijacked };

/**
 * The value to write for TIDAL's `src` on a `<source>`: ours for a cover, the value otherwise.
 * React writes it before the child has a parent, and the edge is then the default until `load()`
 * corrects it with the parent's.
 */
export function hijackSourceSrc(source: HTMLSourceElement, value: string): string {
	if (!isCoverUrl(value)) {
		delete (source as CoverSource)._lunaCover;
		return value;
	}
	const parent = source.parentElement instanceof HTMLVideoElement ? source.parentElement : null;
	const edge = coverEdge(parent);
	(source as CoverSource)._lunaCover = { real: value, edge };
	return lunaCoverUrl(value, edge);
}

/**
 * At a `<video>`'s `load()`: write the URL its cover now calls for (the edge from the element, the
 * latest bust) through `write`, which must bypass the `setAttribute` hook. A `<source>` written
 * before the hook saw it still carries TIDAL's URL, and is taken over here.
 */
export function retargetCover(media: HTMLMediaElement, write: (source: HTMLSourceElement, url: string) => void) {
	const source = media.querySelector("source") as CoverSource | null;
	if (!source) return;
	const real = source._lunaCover?.real ?? (isCoverUrl(source.src) ? source.src : null);
	if (!real) return;
	const edge = coverEdge(media, real);
	source._lunaCover = { real, edge };
	const url = lunaCoverUrl(real, edge);
	if (source.getAttribute("src") !== url) write(source, url);
}

/** Two covers are not decoded in one: the second waits for the first. */
let queue: Promise<void> = Promise.resolve();
const inFlight = new Set<string>();
/** `failed` per cover: one more after the first heals a bad file, a third would not. */
const MAX_SPENT = 2;
/** This Chromium cannot encode VP9: covers stay still for the session. A codec switched off is
 * not latched here: the switch is live, and the next miss asks again. */
let noEncoder = false;
/** Past this a transcode is abandoned: a wedged encoder or stream must not hold the queue. The
 * grant Rust opens for the upload lives as long. */
const TRANSCODE_DEADLINE_MS = 120_000;
/** The ladder between two passes of one cover: from the first rung, doubled up to the last. */
const FIRST_WAIT_MS = 5_000;
const MAX_WAIT_MS = 30 * 60_000;

/**
 * One cover's standing. Only a `failed` spends; a stored or a transient pass climbs the ladder,
 * which bounds by time what no count can tell apart: a cover the cache lost, a network down, a
 * file that loads then breaks.
 */
interface Standing {
	spent: number;
	wait: number;
	notBefore: number;
	timer?: ReturnType<typeof setTimeout>;
	/** A miss in the floor was put off; once per calm spell, or the put-off pass would chain. */
	deferred: boolean;
}
const standing = new Map<string, Standing>();
const standingOf = (key: string) => {
	let s = standing.get(key);
	if (!s) standing.set(key, (s = { spent: 0, wait: 0, notBefore: 0, deferred: false }));
	return s;
};

const open = (s: Standing) => !noEncoder && s.spent < MAX_SPENT;
const closed = (key: string) => (standing.get(key)?.spent ?? 0) >= MAX_SPENT;

/** A floor further off than the longest wait is a clock set back: it does not bind. */
const cooling = (s: Standing) => {
	const left = s.notBefore - Date.now();
	return left > 0 && left <= MAX_WAIT_MS;
};

/** One rung up. A floor long past, or one a clock has since moved away from, starts over. */
const climb = (s: Standing) => {
	const gap = Date.now() - s.notBefore;
	if (gap > MAX_WAIT_MS || gap < -MAX_WAIT_MS) {
		s.wait = 0;
		s.deferred = false;
	}
	s.wait = s.wait ? Math.min(s.wait * 2, MAX_WAIT_MS) : FIRST_WAIT_MS;
	s.notBefore = Date.now() + s.wait;
};

type Transcoder = (real: string, edge: number, signal?: AbortSignal) => Promise<TranscodeOutcome>;
let transcode: Transcoder = transcodeCover;

/** Tests only: the transcode, and a clean slate. */
export function setTranscoderForTests(fn: Transcoder) {
	transcode = fn;
	queue = Promise.resolve();
	inFlight.clear();
	for (const s of standing.values()) clearTimeout(s.timer);
	standing.clear();
	busts.clear();
	noEncoder = false;
}

/** The queue as it stands, for a test to await. */
export const coverQueueSettled = () => queue;

const elementsShowing = (key: string) =>
	Array.from(document.querySelectorAll("video > source") as NodeListOf<CoverSource>)
		.filter((source) => source._lunaCover && coverKey(source._lunaCover.real, source._lunaCover.edge) === key)
		.map((source) => source.parentElement as HTMLVideoElement);

/** Asked again later, by itself, and only for a cover still on screen. */
const retryLater = (real: string, edge: number, key: string, s: Standing, deferral = false) => {
	s.timer = setTimeout(
		() => {
			s.timer = undefined;
			if (!open(s) || inFlight.has(key) || !elementsShowing(key).length) return;
			// Spent when it runs: one that found nothing on screen leaves the next miss its turn.
			if (deferral) s.deferred = true;
			start(real, edge, key);
		},
		s.notBefore - Date.now(),
	);
};

function enqueue(real: string, edge: number) {
	const key = coverKey(real, edge);
	const s = standingOf(key);
	if (inFlight.has(key) || !open(s)) return;
	if (!cooling(s)) return start(real, edge, key);
	// A miss in the floor (a Clear right after a store, a GPU reset) would leave the cover still
	// until its element remounts: put off to the floor, unless a retry already owns it.
	if (s.timer || s.deferred) return;
	retryLater(real, edge, key, s, true);
}

function start(real: string, edge: number, key: string) {
	const s = standingOf(key);
	// The pass owns the cover now: a retry still pending would run a second one behind it.
	clearTimeout(s.timer);
	s.timer = undefined;
	inFlight.add(key);
	const pass = queue.then(async () => {
		let outcome: TranscodeOutcome;
		const deadline = new AbortController();
		const timer = setTimeout(() => deadline.abort(), TRANSCODE_DEADLINE_MS);
		try {
			outcome = await transcode(real, edge, deadline.signal);
		} catch (e) {
			console.warn("[cover] transcode threw:", e);
			outcome = "failed";
		} finally {
			clearTimeout(timer);
		}
		inFlight.delete(key);
		switch (outcome) {
			case "stored":
				climb(s);
				busts.set(key, nextBust());
				for (const video of elementsShowing(key)) {
					// A foreign wrapper of `load()` that throws must not keep the other elements still.
					try {
						video.load();
					} catch (e) {
						console.error("[cover] reload failed:", e);
					}
				}
				break;
			case "failed":
				// No rung: the second attempt is the one that heals a bad file.
				s.spent++;
				// Closed: its elements retarget to the largest size still open, down to their own.
				if (s.spent >= MAX_SPENT) {
					for (const video of elementsShowing(key)) {
						try {
							video.load();
						} catch (e) {
							console.error("[cover] reload failed:", e);
						}
					}
				}
				break;
			case "retry-soon":
				climb(s);
				retryLater(real, edge, key, s);
				break;
			case "retry-on-miss":
				climb(s);
				break;
			case "no-encoder":
				noEncoder = true;
				break;
			case "unavailable":
				break;
		}
	});
	// One pass that throws must not skip every pass queued behind it.
	queue = pass.catch((e) => console.error("[cover] pass failed:", e));
}

/** The largest size a miss reloaded each element at, per cover: a reload only ever goes up. */
const reloadedTo = new WeakMap<HTMLVideoElement, { real: string; edge: number }>();

/**
 * A request for one of our URLs failed (`url`): the cover is not transcoded yet, or not playable.
 * Only the request the element still holds is acted on: the cover it held before is no longer wanted.
 */
export function coverMissed(target: unknown, url: string) {
	const source = (
		target instanceof HTMLVideoElement ? target.querySelector("source") : target
	) as CoverSource | null;
	const cover = source instanceof HTMLSourceElement ? source._lunaCover : undefined;
	if (!cover) return;
	// Its `load()` requests the new URL, which reports its own miss. Logged: a `currentSrc` that
	// never matched would leave every cover still.
	if (url !== source!.src) {
		console.warn("[cover] miss of a request the element no longer holds:", url);
		return;
	}
	// Whatever comes of the transcode, the URL that failed is never written again.
	busts.set(coverKey(cover.real, cover.edge), nextBust());
	// At the size its element calls for now: React writes the `src` before the `<source>` has its
	// `<video>`, and the first request (and its miss) can carry the default edge.
	const parent = source!.parentElement instanceof HTMLVideoElement ? source!.parentElement : null;
	const edge = parent ? coverEdge(parent, cover.real) : cover.edge;
	// Larger than the file that failed: that size may be cached. It is asked for first, and its own
	// miss transcodes. At most one reload per published size, per element and cover.
	const reloaded = parent ? reloadedTo.get(parent) : undefined;
	const reached = reloaded?.real === cover.real ? reloaded.edge : 0;
	if (parent && servedEdge(edge) > Math.max(servedEdge(cover.edge), reached)) {
		reloadedTo.set(parent, { real: cover.real, edge: servedEdge(edge) });
		cover.edge = edge;
		try {
			parent.load();
		} catch (e) {
			console.error("[cover] reload failed:", e);
		}
		return;
	}
	// The reload after the store finds the element by it.
	cover.edge = edge;
	enqueue(cover.real, edge);
}

window.addEventListener("luna-cover-miss", (e) => {
	const { target, url } = (e as CustomEvent<{ target: unknown; url: string }>).detail;
	coverMissed(target, url);
});
