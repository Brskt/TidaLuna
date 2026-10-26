// Tests `early-runtime/cover_error.js` against `isCoverUrl`, the predicate it mirrors: a cover's
// failure is kept from TIDAL, every other error goes through.

import { expect, test } from "bun:test";

import { fragmentSource, runFragment } from "./helpers/early-runtime";
import { isCoverUrl } from "../src/video/cover-canvas";

const source = await fragmentSource("cover_error");

const install = () => {
	let listener: ((event: unknown) => void) | undefined;
	let capture: unknown;
	const window = {
		addEventListener: (type: string, fn: (event: unknown) => void, useCapture: unknown) => {
			if (type !== "error") return;
			listener = fn;
			capture = useCapture;
		},
	};
	runFragment(source, { window });
	return { listener: listener!, capture };
};

// The error is non-bubbling and React listens on the `<source>` itself: only a capture on
// `window` gets there first.
test("a cover's error is kept from the page exactly when isCoverUrl names it", () => {
	const { listener, capture } = install();
	expect(capture).toBe(true);

	const urls = [
		"https://resources.tidal.com/videos/a/b/640x640.mp4",
		"https://resources.tidal.com/images/a/b/1280x1280.jpg",
		"https://lgf.audio.tidal.com/mediatracks/a",
		"http://resources.tidal.com/videos/a/640x640.mp4",
		"https://evil.example/videos/a.mp4",
		"",
	];
	for (const url of urls) {
		const targets = {
			source: { localName: "source", src: url },
			"video selected": { localName: "video", currentSrc: url, src: "" },
			"video src": { localName: "video", currentSrc: "", src: url },
			img: { localName: "img", src: url },
		};
		for (const [name, target] of Object.entries(targets)) {
			let stopped = false;
			listener({ target, stopImmediatePropagation: () => (stopped = true) });
			expect({ name, url, stopped }).toEqual({ name, url, stopped: name !== "img" && isCoverUrl(url) });
		}
	}
});
