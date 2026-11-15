// The play and pause intent the SDK's native-player delegate (`src/controllers/player.ts`) last sent
// Rust, once Rust moves the transport on its own.
//
// Regression: a native Stop (the menu's, the OS media keys') left that intent at "playing"; the
// listener's next Play was dropped as a repeat and never reached Rust.

import { beforeEach, expect, mock, test } from "bun:test";
import type { SentIpc } from "./setup";

mock.module("../plugins/lib/src/redux/store", () => ({
	store: {
		getState: () => ({ playQueue: { currentIndex: 0, elements: [] }, playbackControls: {}, content: { mediaItems: {} } }),
		dispatch: () => {},
		subscribe: () => () => {},
	},
}));

const { createNativePlayerComponent } = await import("../src/controllers/player");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const count = (name: string) => sent.filter((s) => s.channel === name).length;

let component: any;

beforeEach(() => {
	component = createNativePlayerComponent();
	component.Player();
	sent.length = 0;
});

test("a Play after Rust paused, stopped or completed on its own reaches Rust", () => {
	// Each state alone: exclusive and ASIO report a native Stop before any `paused`, a track's end
	// comes with none, and a Connect controller's pause with no `stopped` behind it.
	for (const state of ["paused", "stopped", "completed"]) {
		component = createNativePlayerComponent();
		component.Player();
		sent.length = 0;
		component.setDesiredPlayback(true);
		component.trigger("mediastate", state, 4);
		component.setDesiredPlayback(true);

		expect(count("player.play")).toBe(2);
	}
});

test("a Pause after a Connect controller resumed the player reaches Rust", () => {
	component.setDesiredPlayback(false);
	component.trigger("mediastate", "active", 4);
	component.setDesiredPlayback(false);

	expect(count("player.pause")).toBe(2);
});

test("a stopped state from a superseded seq leaves the sent intent in place", () => {
	component.trigger("mediastate", "active", 5);
	component.setDesiredPlayback(true);
	component.trigger("mediastate", "stopped", 3);
	component.setDesiredPlayback(true);

	expect(count("player.play")).toBe(1);
});
