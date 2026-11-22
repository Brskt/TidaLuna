// Tests `early-runtime/init_static.js`, which runs in the early-runtime payload before TIDAL's page
// exists. A wrong value is silent: `index` then picks the web client and the session it can't refresh.

import { expect, test } from "bun:test";

import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("init_static");

test("TIDAL's own inline config cannot turn the desktop features off", () => {
	const window: any = {};
	runFragment(source, { window });

	// What TIDAL's index.html does before its modules load.
	window.TIDAL_CONFIG = {};
	window.TIDAL_CONFIG.enableDesktopFeatures = false;
	window.TIDAL_CONFIG = { enableDesktopFeatures: false, region: "eu" };

	expect(window.TIDAL_CONFIG.enableDesktopFeatures).toBe(true);
	expect(window.TIDAL_CONFIG.region).toBe("eu");
});
