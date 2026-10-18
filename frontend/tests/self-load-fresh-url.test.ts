// Drives `self-load-fresh-url.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and `playback-staleness.test.ts` replaces `getPlaybackInfo` for every file that runs after it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("a track replayed hours later is loaded from a freshly signed url", () =>
	runIsolated("self-load-fresh-url", "self-load"));
