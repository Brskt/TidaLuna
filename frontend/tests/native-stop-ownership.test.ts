// Drives `native-stop-ownership.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and the stand-ins for the store and `getPlaybackInfo` that file installs would reach every file
// that runs after it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("the SDK native player's stop reaches Rust only for a track that player holds", () =>
	runIsolated("native-stop-ownership", "native stop ownership"));
