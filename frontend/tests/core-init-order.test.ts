// Drives `core-init-order.isolated.ts` in its own Bun process, and exists only because that
// test cannot run beside the others: Bun shares one module registry across the files of a run,
// `mock.module` only takes effect for a module nobody has imported yet, and two other test
// files pull in the real `render/src/index.ts` transitively. Run together, the mock is a no-op
// and the assertion passes or fails on file order rather than on the code, which is how this
// arrangement announced itself: green alone, red in the suite, for no reason in the subject.

import { test } from "bun:test";
import { runIsolated } from "./helpers/isolated";

test("initCore publishes the store binding before the config seed", () =>
	runIsolated("core-init-order", "init-order"));
