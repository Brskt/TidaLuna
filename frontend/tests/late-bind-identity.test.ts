// Drives `late-bind-identity.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and the stand-in for `getPlaybackInfo` that file installs would reach every file that runs after
// it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("the SDK's load of a track the listener left is answered with its own length", () =>
	runIsolated("late-bind-identity", "late-bind identity"));
