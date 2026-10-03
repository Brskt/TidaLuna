// Tests for the Connect receiver's volume and mute relays (`src/connect/index.ts`).
//
// TIDAL re-applying a level Rust mirrored into its store must not send it back.
//
// The store runs TIDAL's own `playbackControls` rules (`helpers/playback-controls.ts`).
// `sdkApplies` stands for Hwe reaching Rust (basePlayer `updateVolumeLevel` needs a stream info).

import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { type Controls, playbackControls } from "./helpers/playback-controls";
import { standInDelegate } from "./helpers/stand-in-delegate";
import type { SentIpc } from "./setup";

let persisted: Controls | null = null;
let sdkApplies = false;
const tidal = playbackControls((volume) => {
	// Z8
	persisted = { ...controls };
	// Hwe, when TIDAL's active player has a stream to apply it to
	if (sdkApplies) applyVolume(volume);
});
const { controls, store } = tidal;
const storeStub = () => ({ store });
mock.module("../plugins/lib/src/redux/store", storeStub);

const { setupConnectEventListeners } = await import("../src/connect");
const { adoptRustVolume, applyVolume, followCommittedVolume } = await import("../src/volume");
const { setSelfLoad } = await import("../src/audio-proxy");
const { rustReport } = await import("./helpers/rust-level");

const sent: SentIpc[] = (globalThis as any).__LUNAR_TEST_IPC__;
const volumes = () => sent.filter((s) => s.channel === "player.volume").map((s) => s.args[0]);
const emit = (channel: string, data: unknown) => (window as any).__LUNAR_IPC_EMIT__(channel, data);

// What Rust reports after applying a controller's `setMute` or `setVolume` (`receiver/mod.rs`).
const rustMute = (mute: boolean, volume: number, unmuteVolume: number) =>
	emit("connect.receiver.mute_changed", { ...rustReport(volume, unmuteVolume), mute });
const rustVolume = (volume: number) =>
	emit("connect.receiver.volume_changed", rustReport(volume, volume));

// The event bus is window state every test file shares: a file that loads `src/index.ts` leaves
// relays bound to its own fake store, and the first relay to mirror a report is the one that
// takes it in. These tests start from their own relays alone.
const ipcListeners = (window as any).__LUNAR_IPC_LISTENERS__;
delete ipcListeners["connect.receiver.volume_changed"];
delete ipcListeners["connect.receiver.mute_changed"];

// Registers the listeners once; the bootstrap snapshot it then awaits never answers here.
void setupConnectEventListeners(store);

// The forwarder's last level is module state, as Rust's is: every test starts from a store and a
// Rust that agree on a level.
function start(volume: number, { sdk }: { sdk: boolean }) {
	// The level is set here without a notification, which a follower subscribed earlier would take
	// for a change: a file that loads `src/index.ts` can finish its init while this one runs and
	// leave its follower on this store. Each test starts from the subscribers it installs.
	tidal.reset({ volume, volumeUnmute: volume, muted: false });
	persisted = null;
	sdkApplies = sdk;
	adoptRustVolume(volume);
	sent.length = 0;
}

beforeEach(() => {
	mock.module("../plugins/lib/src/redux/store", storeStub);
});
// `cast()` announces through whatever delegate is on `window`.
standInDelegate();

test("a remote mute shows muted, keeps Rust's level to restore, and persists both", () => {
	start(70, { sdk: false });
	rustMute(true, 0, 70);
	expect(controls).toEqual({ volume: 0, volumeUnmute: 70, muted: true });
	expect(persisted).toEqual({ volume: 0, volumeUnmute: 70, muted: true });
	expect(volumes()).toEqual([]);
});

test("a remote unmute shows the level Rust restored", () => {
	start(64, { sdk: false });
	rustMute(true, 0, 64);
	rustMute(false, 64, 64);
	expect(controls).toEqual({ volume: 64, volumeUnmute: 64, muted: false });
	expect(volumes()).toEqual([]);
});

test("a remote unmute takes Rust's level over the store's own memory", () => {
	// The store missed a change Rust applied (no page was loaded); its memory is older. TIDAL's
	// SET_MUTE saga would restore that one, and send it to Rust whenever the SDK has a stream.
	start(30, { sdk: true });
	rustMute(true, 0, 80);
	rustMute(false, 80, 80);
	expect(controls.volume).toBe(80);
	expect(controls.muted).toBe(false);
	expect(volumes()).toEqual([]);
});

test("a mute repeated by the controller keeps the level to restore", () => {
	start(55, { sdk: false });
	rustMute(true, 0, 55);
	rustMute(true, 0, 55);
	expect(controls).toEqual({ volume: 0, volumeUnmute: 55, muted: true });
});

test("TIDAL re-applying a mirrored mute or unmute sends nothing back to Rust", () => {
	start(50, { sdk: true });
	rustMute(true, 0, 50);
	rustMute(false, 50, 50);
	expect(volumes()).toEqual([]);
});

test("TIDAL re-applying a mirrored remote volume sends nothing back to Rust", () => {
	start(30, { sdk: true });
	rustVolume(42.4);
	expect(controls.volume).toBe(42);
	expect(volumes()).toEqual([]);
});

test("after a remote volume, the slider moved back to the old level still reaches Rust", () => {
	// Rust holds 40 now. A road that still believes Rust holds 70 would drop the user's 70.
	start(70, { sdk: false });
	rustVolume(40);
	sdkApplies = true;
	store.dispatch({ type: "playbackControls/SET_VOLUME", payload: { volume: 70 } });
	expect(volumes()).toEqual([70]);
});

test("the mute button after a remote mute restores the level Rust would", () => {
	// The store still shows 40 when the phone mutes a Rust playing at 55.
	start(40, { sdk: true });
	rustMute(true, 0, 55);
	store.dispatch({ type: "playbackControls/TOGGLE_MUTE" });
	expect(controls.volume).toBe(55);
	expect(volumes()).toEqual([55]);
});

// Rust plays the phone's track. TIDAL's SDK holds that track's stream from each announcement's load
// until the track completes, and none in between.

const slider = (volume: number) => store.dispatch({ type: "playbackControls/SET_VOLUME", payload: { volume } });
const muteButton = () => store.dispatch({ type: "playbackControls/TOGGLE_MUTE" });
// Rust started the phone's track: `receiver/mod.rs` emits this after every receiver load.
const cast = () => emit("connect.receiver.media_changed", { mediaId: "1" });

let unfollow: (() => void) | undefined;

// As `start`, from a local load, with the road that follows the committed level installed.
function startLocal(volume: number, { sdk }: { sdk: boolean }) {
	start(volume, { sdk });
	setSelfLoad(false);
	unfollow = followCommittedVolume(store);
}

afterEach(() => {
	unfollow?.();
	unfollow = undefined;
});

test("the slider during a cast reaches Rust while TIDAL's SDK holds no stream", () => {
	startLocal(70, { sdk: false });
	cast();
	slider(35);
	expect(volumes()).toEqual([35]);
});

test("the mute button during a cast mutes Rust, and unmutes it at the level it had", () => {
	startLocal(60, { sdk: false });
	cast();
	muteButton();
	expect(volumes()).toEqual([0]);
	muteButton();
	expect(volumes()).toEqual([0, 60]);
});

test("a local mute during a cast, then the phone's unmute, sends nothing back", () => {
	startLocal(60, { sdk: false });
	cast();
	muteButton();
	// Rust kept 60 to restore (`OutputLevel`), and reports it.
	rustMute(false, 60, 60);
	expect(controls).toEqual({ volume: 60, volumeUnmute: 60, muted: false });
	expect(volumes()).toEqual([0]);
});

test("a cast that follows a self-loaded track takes the slider too", () => {
	startLocal(70, { sdk: false });
	setSelfLoad(true);
	cast();
	slider(25);
	expect(volumes()).toEqual([25]);
});

test("while the SDK holds the cast track's stream and applies it, a change reaches Rust once", () => {
	startLocal(70, { sdk: true });
	cast();
	slider(45);
	muteButton();
	expect(volumes()).toEqual([45, 0]);
});

test("the phone's own level, mirrored into the store during a cast, is not sent back", () => {
	startLocal(30, { sdk: false });
	cast();
	rustVolume(42);
	rustMute(true, 0, 42);
	expect(volumes()).toEqual([]);
	// The slider back to the level Rust held before still reaches it.
	slider(30);
	expect(volumes()).toEqual([30]);
});

test("a level set before anything plays reaches Rust", () => {
	startLocal(70, { sdk: false });
	slider(40);
	expect(volumes()).toEqual([40]);
});

test("a change made before the cast reaches Rust once, and is not replayed when it starts", () => {
	startLocal(70, { sdk: false });
	slider(50);
	cast();
	store.dispatch({ type: "playbackControls/SET_VOLUME_UNMUTE", payload: { volume: 50 } });
	expect(volumes()).toEqual([50]);
});

test("a change before TIDAL's SDK holds a stream, then its own application, reaches Rust once", () => {
	startLocal(70, { sdk: false });
	cast();
	// The delegate's `load` for a track picked here: its stream starts after the change.
	setSelfLoad(false);
	slider(20);
	sdkApplies = true;
	applyVolume(20);
	expect(volumes()).toEqual([20]);
});

test("a local change during a cast carries the count of the phone's last level the page took in", () => {
	// `OutputLevel::admits` refuses a page level whose count predates the floor.
	startLocal(70, { sdk: false });
	cast();
	const phone = rustReport(40, 40);
	emit("connect.receiver.volume_changed", phone);
	slider(35);
	expect(sent.filter((s) => s.channel === "player.volume").map((s) => s.args)).toEqual([[35, phone.seq]]);
});
