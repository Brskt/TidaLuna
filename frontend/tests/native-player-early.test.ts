// TIDAL's About reads the native player's version through a listener its index chunk attaches
// while it evaluates, before our bundle defines `NativePlayerComponent`. The early runtime's
// stand-in is what that listener meets, and a wrong line in About is silent.

import { expect, test } from "bun:test";

import { fragmentSource, runFragment } from "./helpers/early-runtime";

const stubSource = await fragmentSource("session_stub");

type VersionListener = (event: { target: unknown }) => void;
type EarlyPlayer = { addEventListener(type: string, listener: VersionListener): void };
type EarlyWindow = { NativePlayerComponent?: { Player(): EarlyPlayer } };

function earlyWindow(cfg: Record<string, unknown>): EarlyWindow {
	const window: EarlyWindow = {};
	runFragment(stubSource, { window, self: window, _cfg: cfg, sendIpc: () => {} });
	return window;
}

// TIDAL's index chunk at evaluation, names aside: `ma` is what About prints after `NP:`.
function tidalIndexEval(window: EarlyWindow): { ma?: string } {
	const seen: { ma?: string } = {};
	if ("NativePlayerComponent" in window && window.NativePlayerComponent) {
		seen.ma = "NA";
		let ha: EarlyPlayer | undefined = window.NativePlayerComponent.Player();
		ha.addEventListener("version", ({ target }) => {
			seen.ma = String(target);
			ha = undefined;
		});
	}
	return seen;
}

test("About names the native player version the index listened for at evaluation", async () => {
	const seen = tidalIndexEval(earlyWindow({ nativePlayerVersion: "9.9.9-test" }));
	// Told after the listener is attached: `Player()` returns before TIDAL can listen.
	expect(seen.ma).toBe("NA");
	await Promise.resolve();
	expect(seen.ma).toBe("9.9.9-test");
});
