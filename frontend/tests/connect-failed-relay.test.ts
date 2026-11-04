// Rust reports a connect it could not complete on `connect.connect_failed`. TIDAL's queue restore
// waits on CONNECT_TO_DEVICE_FAILED when a device was persisted: a relay missing here leaves it
// waiting, with nothing in any log.

import { expect, test } from "bun:test";
import { setupConnectEventListeners } from "../src/connect";

test("a connect Rust failed reaches TIDAL's store as CONNECT_TO_DEVICE_FAILED", () => {
	const dispatched: unknown[] = [];
	// The listeners register before the snapshot read, which this test leaves unanswered.
	void setupConnectEventListeners({ dispatch: (action) => dispatched.push(action), getState: () => ({}) });

	(window as any).__LUNAR_IPC_EMIT__("connect.connect_failed");

	expect(dispatched).toEqual([{ type: "remotePlayback/CONNECT_TO_DEVICE_FAILED" }]);
});
