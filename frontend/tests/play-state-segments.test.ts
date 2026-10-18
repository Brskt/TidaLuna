// Drives `play-state-segments.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and the stand-ins for `@luna/core`, the redux module and `MediaItem` that file installs would reach every file that
// runs after it. It also replaces `performance.now` and `Date.now`, which no other file should see.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test(
	"the time PlayState counts for scrobbling belongs to the track it was played on, in whichever order the flows arrive",
	() => runIsolated("play-state-segments", "segments"),
	20000,
);
