// Drives `media-item-fresh-url.isolated.ts` in its own Bun process: `mock.module` is process-wide,
// and the classes that file replaces would reach every file that runs after it.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("a download hours after the first ask uses freshly signed urls", () =>
	runIsolated("media-item-fresh-url", "media-item"));
