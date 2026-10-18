// Runs `tests/<name>.isolated.ts` in its own Bun process, for a file whose stand-ins or boot would
// reach every file that runs after it in the shared one.
//
// The isolated files are named without `.test.` to keep `bun test tests/` from collecting them; the
// `./` prefix below is what makes Bun read the argument as a path instead of a name filter.

import { expect } from "bun:test";
import { fileURLToPath } from "url";

export async function runIsolated(name: string, what: string): Promise<void> {
	const proc = Bun.spawn(["bun", "test", `./tests/${name}.isolated.ts`], {
		// A URL's pathname is not a filesystem path: on Windows it reads `/C:/...`.
		cwd: fileURLToPath(new URL("../..", import.meta.url)),
		stdout: "pipe",
		stderr: "pipe",
	});
	const [out, err, code] = await Promise.all([
		new Response(proc.stdout).text(),
		new Response(proc.stderr).text(),
		proc.exited,
	]);
	expect(code, `the isolated ${what} test failed:\n${out}${err}`).toBe(0);
}
