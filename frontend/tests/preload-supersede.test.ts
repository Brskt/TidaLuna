// Drives `preload-supersede.isolated.ts` in its own Bun process: that file boots `src/index.ts` over
// a stand-in store, and what the boot registers (the hand-over and bind listeners of `audio-proxy.ts`, the
// interceptors) would reach every file that runs after it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("a next that leaves the native player's staged track behind is put behind an unset", () =>
	runIsolated("preload-supersede", "preload supersede"));
