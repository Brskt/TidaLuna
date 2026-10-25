// How the page finds the player SDK's API (`src/sdk-player.ts`): among the exports the Rust filter captured
// from the chunk that defines it, by the shape of what they are, as the minified name of the export changes
// with every build.

import { afterEach, beforeEach, expect, test } from "bun:test";
import { SDK_PLAYER_API, findSdkPlayer, sdkPlayer, startSdkPlayer } from "../src/sdk-player";

const api = () => ({
	setNext: () => Promise.resolve(),
	load: () => Promise.resolve(),
	reset: () => Promise.resolve(),
	setEventSender: () => {},
});

const quiet = <T>(run: () => T): { result: T; warned: unknown[][] } => {
	const warned: unknown[][] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void warned.push(args);
	try {
		return { result: run(), warned };
	} finally {
		console.warn = original;
	}
};

const host = (globalThis as { __lunaHostModules?: Record<string, unknown> });
let before: Record<string, unknown> | undefined;
beforeEach(() => {
	before = host.__lunaHostModules;
	host.__lunaHostModules = {};
});
afterEach(() => {
	host.__lunaHostModules = before;
	quiet(() => startSdkPlayer(() => null));
});

test("the SDK's API is the captured export that carries its functions, whatever its name", () => {
	const sdk = api();

	expect(findSdkPlayer({ [SDK_PLAYER_API]: { F: () => {}, w: sdk, x: { setNext: () => {} }, y: 4 } })).toBe(sdk as any);
});

test("an export with some of the functions is not the SDK's API", () => {
	expect(findSdkPlayer({ [SDK_PLAYER_API]: { a: { setNext: () => {}, load: () => {} }, b: { reset: () => {} }, c: null } })).toBeNull();
});

test("the API is looked for again until it is found, then kept", () => {
	quiet(() => startSdkPlayer());
	expect(sdkPlayer()).toBeNull();

	const sdk = api();
	host.__lunaHostModules = { [SDK_PLAYER_API]: { w: sdk } };
	expect(sdkPlayer()).toBe(sdk as any);

	host.__lunaHostModules = {};
	expect(sdkPlayer()).toBe(sdk as any);
});
