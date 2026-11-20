// Rust rewrites Shaka's MediaSource test to false and has the vendor chunk set
// `globalThis.__lunaShakaBlind` as its last statement. A MediaSource is only safe to show once that ran:
// a Shaka that sees one builds MSE on every audio element too.

/**
 * Runs `install` once the rewritten chunk has set the flag, before or after this call; never if the
 * rewrite did not match. The bundle is injected once the page has loaded, after TIDAL's static
 * chunks: the vendor chunk has set it already, a chunk imported later sets it through the setter.
 */
export function armOnShakaBlind(install: () => void): void {
	let blind = false;
	const blindNow = () => {
		blind = true;
		try {
			install();
		} catch (e) {
			// Thrown from here it would abort bootstrap, or the chunk setting the flag, mid-evaluation.
			console.error("[video] the transcoding MediaSource did not install:", e);
		}
	};
	const alreadySet = Boolean((globalThis as any).__lunaShakaBlind);
	Object.defineProperty(globalThis, "__lunaShakaBlind", {
		configurable: true,
		get: () => blind,
		set: (value: unknown) => {
			if (blind || !value) return;
			blindNow();
		},
	});
	if (alreadySet) blindNow();
}
