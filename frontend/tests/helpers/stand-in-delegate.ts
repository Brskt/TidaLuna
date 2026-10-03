// Puts a stand-in delegate on `window` for every test of the calling file, and the one found
// there back after each: the real one another file left would run a playback controller whose
// timers fire in whichever file comes next. A test may replace it with one of its own.

import { afterEach, beforeEach } from "bun:test";

export function standInDelegate(): void {
	let previous: unknown;
	beforeEach(() => {
		previous = (window as any).nativeInterface;
		(window as any).nativeInterface = { playback: { setCurrentMediaItem: () => {} } };
	});
	afterEach(() => {
		(window as any).nativeInterface = previous;
	});
}
