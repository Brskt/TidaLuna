// Drives `self-load-credential-refresh.isolated.ts` in its own Bun process: `mock.module` is
// process-wide, and the stand-in for `getPlaybackInfo` that file installs would reach every file
// that runs after it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

// One of its tests waits out the bound on the credential request, 3 s.
test(
	"a reload of the self-loaded track hands Rust a fresh credential first",
	() => runIsolated("self-load-credential-refresh", "credential-refresh"),
	20000,
);
