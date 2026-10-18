// Tests the `loadstart` entry of the animated cover: `src/audio-proxy.ts` listens for it on the
// document and hands the element to `coverSelected` (`src/video/cover-canvas.ts`).
//
// This is the only way a `<video>` that TIDAL remounts WITHOUT a `load()` (after an error) reaches
// the cover canvas: nothing else in the page observes it before its surface is held. happy-dom
// never emits `loadstart` itself, on `load()`, on a `src` or on a `<source>` going in; each test
// dispatches the event by hand, the way the engine does: not bubbling.
//
// Its own file because importing `audio-proxy` installs its hooks on `HTMLMediaElement` and the
// document for the whole process, which `cover-canvas.test.ts` has no use for. Every test names a
// cover of its own: `coverCache` in the module outlives the test.

import { afterEach, beforeEach, expect, test } from "bun:test";

import "../src/audio-proxy";
import { refusingTransport } from "./helpers/refusing-transport";

const coverSrc = (name: string) => `https://resources.tidal.com/videos/${name}/640x640.mp4`;

const realFetch = globalThis.fetch;
const realRaf = globalThis.requestAnimationFrame;

let requested: string[] = [];

// The answer never comes: the canvas the attach lays down before its fetch stays where a test can
// see it, and no draw loop or give-up runs behind the test.
const transportNeverAnswering = () => {
	(globalThis as any).fetch = (url: string) => {
		requested.push(decodeURIComponent(url));
		return new Promise(() => {});
	};
};

const transportRefusing = () => refusingTransport((url) => requested.push(decodeURIComponent(url)));

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

// TIDAL's artwork frame holding a media element whose selected resource is on a `<source>` child,
// as it mounts a cover.
const mountFrame = (tag: "video" | "audio", src: string) => {
	const surface = document.createElement("div");
	const media = document.createElement(tag);
	const source = document.createElement("source");
	source.setAttribute("src", src);
	media.appendChild(source);
	surface.appendChild(media);
	document.body.appendChild(surface);
	return { surface, media };
};

// What the engine fires when a resource selection starts: it does not bubble.
const loadstart = () => new Event("loadstart");

beforeEach(() => {
	requested = [];
	(globalThis as any).requestAnimationFrame = () => 0;
});

afterEach(() => {
	globalThis.fetch = realFetch;
	globalThis.requestAnimationFrame = realRaf;
	document.body.replaceChildren();
});

test("a loadstart on a cover video attaches its cover through the document's listener", () => {
	transportNeverAnswering();
	const { surface, media } = mountFrame("video", coverSrc("ls-listener"));

	const event = loadstart();
	expect(event.bubbles).toBe(false);
	media.dispatchEvent(event);

	expect(requested.length).toBe(1);
	expect(requested[0]).toContain(coverSrc("ls-listener"));
	expect(surface.querySelectorAll("canvas").length).toBe(1);
});

test("the loadstart of a selection already handed over is not attached a second time", async () => {
	transportRefusing();
	const { media } = mountFrame("video", coverSrc("ls-twice"));

	media.dispatchEvent(loadstart());
	await settle();
	media.dispatchEvent(loadstart());
	await settle();

	expect(requested.length).toBe(1);
});

test("a loadstart that is not a cover video's attaches nothing", () => {
	transportNeverAnswering();
	const audio = mountFrame("audio", coverSrc("ls-audio"));
	const other = mountFrame("video", "https://resources.tidal.com/images/a/b/640x640.jpg");
	const bare = document.createElement("div");
	document.body.appendChild(bare);

	audio.media.dispatchEvent(loadstart());
	other.media.dispatchEvent(loadstart());
	bare.dispatchEvent(loadstart());

	expect(requested.length).toBe(0);
	expect(document.querySelectorAll("canvas").length).toBe(0);
});
