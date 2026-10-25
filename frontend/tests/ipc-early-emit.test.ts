// Tests `early-runtime/ipc.js`, the IPC listener store the page has before the bundle runs.
// Its EMIT is the one live until the bundle takes over: a listener that throws there must
// leave a trace, as the bundle's own EMIT does in `src/ipc.ts`.

import { expect, test } from "bun:test";
import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("ipc");

function boot() {
	const scope: Record<string, any> = {};
	const errors: unknown[][] = [];
	runFragment(source, {
		window: { cefQuery: () => {} },
		self: scope,
		console: { error: (...a: unknown[]) => errors.push(a), warn: () => {} },
	});
	return { scope, errors };
}

test("a throwing early listener is logged and the next one still runs", () => {
	const { scope, errors } = boot();
	const ran: string[] = [];
	scope.__LUNAR_IPC_ON__("ch", () => {
		throw new Error("boom");
	});
	scope.__LUNAR_IPC_ON__("ch", () => ran.push("second"));
	scope.__LUNAR_IPC_EMIT__("ch", 1);
	expect(ran).toEqual(["second"]);
	expect(errors.length).toBe(1);
	expect(errors[0][0]).toBe("[luna:ipc] event handler error:");
	expect(errors[0][1]).toBe("ch");
});
