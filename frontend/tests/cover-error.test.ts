// Tests `early-runtime/cover_error.js` against `isCoverUrl`, the predicate it mirrors: a cover's
// failure is kept from TIDAL, every other error goes through, and a failure of our transcode URL
// reaches the bundle instead.

import { expect, test } from "bun:test";

import { fragmentSource, runFragment } from "./helpers/early-runtime";
import { isCoverUrl } from "../src/video/cover-hijack";

const source = await fragmentSource("cover_error");

const install = () => {
	let listener: ((event: unknown) => void) | undefined;
	let capture: unknown;
	const missed: unknown[] = [];
	const window = {
		addEventListener: (type: string, fn: (event: unknown) => void, useCapture: unknown) => {
			if (type !== "error") return;
			listener = fn;
			capture = useCapture;
		},
		dispatchEvent: (event: CustomEvent) => {
			if (event.type === "luna-cover-miss") missed.push(event.detail);
			return true;
		},
	};
	runFragment(source, { window, CustomEvent });
	return { listener: listener!, capture, missed };
};

const TRANSCODE = "luna-cover://video/?src=https%3A%2F%2Fresources.tidal.com%2Fvideos%2Fa%2F640x640.mp4&edge=640";

// The error is non-bubbling and React listens on the `<source>` itself: only a capture on
// `window` gets there first.
test("a cover's error is kept from the page, and only our transcode's reaches the bundle", () => {
	expect(install().capture).toBe(true);
	const urls = [
		TRANSCODE,
		`${TRANSCODE}&r=17`,
		"https://resources.tidal.com/videos/a/b/640x640.mp4",
		"https://resources.tidal.com/images/a/b/1280x1280.jpg",
		"https://lgf.audio.tidal.com/mediatracks/a",
		"http://resources.tidal.com/videos/a/640x640.mp4",
		"https://evil.example/videos/a.mp4",
		"luna-cover://frames/?src=x",
		"",
	];
	for (const url of urls) {
		const targets = {
			source: { localName: "source", src: url, parentElement: { currentSrc: url } },
			"video selected": { localName: "video", currentSrc: url, src: "" },
			"video src": { localName: "video", currentSrc: "", src: url },
			img: { localName: "img", src: url },
		};
		for (const [name, target] of Object.entries(targets)) {
			const { listener, missed } = install();
			let stopped = false;
			listener({ target, stopImmediatePropagation: () => (stopped = true) });

			const ours = name !== "img" && url.startsWith("luna-cover://video/");
			expect({ name, url, stopped, missed }).toEqual({
				name,
				url,
				stopped: name !== "img" && (ours || isCoverUrl(url)),
				missed: ours ? [{ target, url }] : [],
			});
		}
	}
});

// React can write the next cover before the old request's error runs; `cover-hijack.ts` tells the two
// apart only if the miss names the request that failed.
test("a miss names the request that failed, not the one the source holds now", () => {
	const later = `${TRANSCODE}&r=18`;
	for (const [parentElement, url] of [
		[{ currentSrc: TRANSCODE }, TRANSCODE],
		// Detached from its `<video>`: no request to name, and the error is still kept from TIDAL.
		[null, ""],
	] as const) {
		const { listener, missed } = install();
		const target = { localName: "source", src: later, parentElement };
		let stopped = false;
		listener({ target, stopImmediatePropagation: () => (stopped = true) });
		expect({ stopped, missed }).toEqual({ stopped: true, missed: [{ target, url }] });
	}
});
