// What `nativeInterface.application` tells TIDAL's page about its host. The page names the desktop
// build from `getVersion()` in About and in its error telemetry, where a wrong value is silent.

import { expect, test } from "bun:test";

import { createApplicationController } from "../src/controllers/application";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const stubSource = await fragmentSource("session_stub");

function earlyApplication(window: Record<string, unknown>) {
	runFragment(stubSource, { window, self: window, _cfg: {}, sendIpc: () => {} });
	return (window.nativeInterface as { application: Record<string, () => unknown> }).application;
}

test("both copies name the TidaLunar build Rust publishes, never an official TIDAL one", () => {
	const g = window as unknown as Record<string, unknown>;
	g.__TIDALUNAR_HOST_VERSION__ = "TidaLunar/9.8.7-test";
	try {
		expect(createApplicationController().getVersion()).toBe("TidaLunar/9.8.7-test");
		expect(earlyApplication({ __TIDALUNAR_HOST_VERSION__: "TidaLunar/9.8.7-test" }).getVersion()).toBe(
			"TidaLunar/9.8.7-test",
		);
	} finally {
		delete g.__TIDALUNAR_HOST_VERSION__;
	}
});
