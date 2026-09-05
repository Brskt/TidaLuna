// Drives `core-init-order.isolated.ts` in its own Bun process, and exists only because that
// test cannot run beside the others: Bun shares one module registry across the files of a run,
// `mock.module` only takes effect for a module nobody has imported yet, and two other test
// files pull in the real `render/src/index.ts` transitively. Run together, the mock is a no-op
// and the assertion passes or fails on file order rather than on the code, which is how this
// arrangement announced itself: green alone, red in the suite, for no reason in the subject.
//
// The isolated file is named without `.test.` to keep `bun test tests/` from collecting it; the
// `./` prefix below is what makes Bun read the argument as a path instead of a name filter.

import { expect, test } from "bun:test";

test("initCore publishes the store binding before the config seed", async () => {
	const proc = Bun.spawn(["bun", "test", "./tests/core-init-order.isolated.ts"], {
		cwd: new URL("..", import.meta.url).pathname,
		stdout: "pipe",
		stderr: "pipe",
	});
	const [out, err, code] = await Promise.all([
		new Response(proc.stdout).text(),
		new Response(proc.stderr).text(),
		proc.exited,
	]);
	expect(code, `the isolated init-order test failed:\n${out}${err}`).toBe(0);
});
