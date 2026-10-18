// Drives `play-state-playtime.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and the stand-ins for `@luna/core` and the redux module that file installs would reach every file that runs after it
// (another file already registers `@luna/core` with a different shape).

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test(
	"the playtime PlayState counts for scrobbling survives a wall clock put back",
	() => runIsolated("play-state-playtime", "playtime"),
	20000,
);
