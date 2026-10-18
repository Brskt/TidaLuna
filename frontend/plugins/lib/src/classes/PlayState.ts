import { registerEmitter, type AddReceiver, type MaybePromise, type VoidLike } from "@inrixia/helpers";

import { findModuleByProperty, type Tracer } from "@luna/core";

import { libTrace, unloads } from "../index.safe";
import * as redux from "../redux";
import { MediaItem } from "./MediaItem";

export class PlayState {
	/**
	 * The minimum duration of a track that needs to be played before it can be scrobbled.
	 * Defaults to 4 minutes in milliseconds
	 */
	public static readonly MIN_SCROBBLE_DURATION = 240000;
	/**
	 * The minimum percentage of a track that needs to be played before it can be scrobbled.
	 * Defaults to 0.5 (50%)
	 */
	public static readonly MIN_SCROBBLE_PERCENTAGE = 0.5;

	/**
	 * The total time the current track has been played in milliseconds
	 * Note: Can be longer than track duration, use `PlayState.playTime` to get the current play time
	 */
	public static cumulativePlaytime: number = 0;
	/**
	 * The time the last track started playing epoch ms
	 */
	public static lastPlayStart?: number;
	// Where `cumulativePlaytime` is counted from. `lastPlayStart` stays an epoch because plugins
	// read it as a timestamp, but the wall clock steps back (a VM's clock sync) and a playtime
	// counted from it would lose the step; this one is monotonic. Set and cleared with it.
	private static playStartMono?: number;
	// The track Rust held when the open segment began, as the bridge names it (`__LUNAR_RUST_PRODUCT_ID__`,
	// set from the `duration` event that opens every load). Rust's states and the page's announcement of a
	// track travel apart and neither waits for the other. A segment is counted for the track it was
	// played on, not for whichever one is announced when it closes. `undefined` when Rust named none:
	// the segment then counts for the announced track.
	private static segmentProduct?: string;
	// Time played on tracks that are not the announced one: the next, whose `active` came before its
	// announcement, and the last, whose `paused` comes after the next one's.
	private static readonly otherPlaytime = new Map<string, number>();

	private static heldProduct(): string | undefined {
		const held = (globalThis as { __LUNAR_RUST_PRODUCT_ID__?: unknown }).__LUNAR_RUST_PRODUCT_ID__;
		return typeof held === "string" ? held : undefined;
	}
	private static countsForCurrent(product: string | undefined): boolean {
		return product === undefined || this.currentMediaItem === undefined || product === String(this.currentMediaItem.id);
	}
	// Adds the open segment's time so far to the track it was played on.
	private static foldOpenSegment(now: number): void {
		if (this.playStartMono === undefined) return;
		const elapsed = now - this.playStartMono;
		if (this.countsForCurrent(this.segmentProduct)) this.cumulativePlaytime += elapsed;
		else this.otherPlaytime.set(this.segmentProduct!, (this.otherPlaytime.get(this.segmentProduct!) ?? 0) + elapsed);
	}

	private static readonly trace: Tracer = libTrace.withSource(".PlayState").trace;
	private static activePlayer?: { currentTime: number };

	public static get playbackControls() {
		return redux.store.getState().playbackControls;
	}
	public static get playbackContext() {
		return this.playbackControls.playbackContext;
	}

	public static get playQueue() {
		return redux.store.getState().playQueue;
	}

	/**
	 * The current playback position in seconds from the active player.
	 * Unlike `playTime` (from Redux state), this is the live player position.
	 */
	public static get currentTime(): number {
		if (this.activePlayer === undefined) {
			const mod = findModuleByProperty<{ activePlayer: { currentTime: number } }>((key, value) => key === "activePlayer" && typeof value === "object" && value !== null && "currentTime" in value);
			this.activePlayer = mod?.activePlayer;
		}
		return this.activePlayer?.currentTime ?? 0;
	}
	public static nextMediaItem() {
		const nextItemId = this.playQueue.elements[this.playQueue.currentIndex + 1]?.mediaItemId;
		return MediaItem.fromId(nextItemId);
	}
	public static previousMediaItem() {
		const previousMediaItem = this.playQueue.elements[this.playQueue.currentIndex - 1]?.mediaItemId;
		return MediaItem.fromId(previousMediaItem);
	}

	public static get state() {
		return this.playbackControls.playbackState;
	}
	public static get desiredState() {
		return this.playbackControls.desiredPlaybackState;
	}

	// #region Shuffle
	public static get shuffle(): boolean {
		return this.playQueue.shuffleModeEnabled;
	}
	public static setShuffle(shuffle: false, unshuffleItems?: boolean): void;
	public static setShuffle(shuffle: true, shuffleItems?: boolean): void;
	public static setShuffle(shuffle: boolean, shuffleItems: boolean = false): MaybePromise<VoidLike> {
		if (shuffleItems)
			return shuffle
				? redux.actions["playQueue/ENABLE_SHUFFLE_MODE_AND_SHUFFLE_ITEMS"]({
					shuffleSeed: Math.random(),
				})
				: redux.actions["playQueue/DISABLE_SHUFFLE_MODE_AND_UNSHUFFLE_ITEMS"]();

		if (shuffle !== this.shuffle) shuffle ? redux.actions["playQueue/ENABLE_SHUFFLE_MODE"]() : redux.actions["playQueue/DISABLE_SHUFFLE_MODE"]();
	}
	// #endregion

	// #region Repeat
	public static RepeatMode = redux.RepeatMode;
	public static get repeatMode(): redux.RepeatMode {
		return this.playQueue.repeatMode;
	}
	public static setRepeatMode(repeatMode: redux.RepeatMode): void {
		redux.actions["playQueue/SET_REPEAT_MODE"](repeatMode);
	}
	// #endregion

	// #region Playback
	public static get playing(): boolean {
		return this.desiredState === "PLAYING";
	}
	/**
	 * Play the current track, or a specific track if mediaItemId is provided.
	 * mediaItemId is not validated, you should use `MediaItem.play`
	 *
	 * If `mediaItemId` is provided, equivalent to:
	 * ```ts
	 * PlayState.playNext(mediaItemId);
	 * PlayState.next();
	 * PlayState.play();
	 * ```
	 */
	public static play(mediaItemId?: redux.ItemId) {
		if (mediaItemId !== undefined) {
			this.playNext(mediaItemId);
			this.next();
		}
		redux.actions["playbackControls/PLAY"]();
	}
	public static pause() {
		redux.actions["playbackControls/PAUSE"]();
	}
	/**
	 * Skip to the next track
	 */
	public static next() {
		redux.actions["playQueue/MOVE_NEXT"]();
	}
	/**
	 * Skip to the previous track
	 */
	public static previous() {
		redux.actions["playQueue/MOVE_PREVIOUS"]();
	}
	/**
	 * Move to a specific track in the play queue
	 * @param playQueueIndex The index of the track in the play queue
	 */
	public static moveTo(playQueueIndex: number) {
		redux.actions["playQueue/MOVE_TO"](playQueueIndex);
	}
	/**
	 * Adds `mediaItemIds` to playQueue after current track as temporary items (removed after played)
	 */
	public static playNext(mediaItemIds: redux.ItemId | redux.ItemId[]) {
		mediaItemIds = Array.isArray(mediaItemIds) ? mediaItemIds : [mediaItemIds];
		redux.actions["playQueue/ADD_NEXT"]({ mediaItemIds, context: { type: "UNKNOWN" } });
	}
	/**
	 * Updeates PlayState.playQueue, note this may cause playback to restart
	 */
	public static updatePlayQueue(playQueue: Partial<redux.PlayQueue>) {
		redux.actions["playQueue/RESET"]({
			...PlayState.playQueue,
			...playQueue,
		});
	}
	// #endregion

	// #region Seek
	public static get playTime() {
		return this.playbackControls.latestCurrentTime;
	}
	public static seek(time: number) {
		redux.actions["playbackControls/SEEK"](time);
	}
	// #endregion

	static {
		// State tracking for scrobbling
		redux.intercept("playbackControls/SET_PLAYBACK_STATE", unloads, (state) => {
			switch (state) {
				case "PLAYING": {
					// A segment still open has nobody to close it when a self-load is replaced by another
					// (no pause is sent), and Rust reports `active` twice for one start.
					const now = performance.now();
					this.foldOpenSegment(now);
					this.lastPlayStart = Date.now();
					this.playStartMono = now;
					this.segmentProduct = this.heldProduct();
					break;
				}
				default: {
					this.foldOpenSegment(performance.now());
					delete this.lastPlayStart;
					delete this.playStartMono;
					delete this.segmentProduct;
				}
			}
		});

	}

	private static currentMediaItem?: MediaItem;
	/**
	 * Triggered on MediaItem.onMediaTransition and when a track should be scrobbled according to `MIN_SCROBBLE_DURATION` and `MIN_SCROBBLE_PERCENTAGE`
	 */
	public static onScrobble: AddReceiver<MediaItem> = registerEmitter(async (onScrobble) => {
		this.currentMediaItem = await MediaItem.fromPlaybackContext();
		let hasScrobbledCurrent = false;

		const checkAndScrobble = (mediaItem: MediaItem): boolean => {
			if (hasScrobbledCurrent) return false;
			if (mediaItem.duration === undefined) return false;

			const longerThan4min = this.cumulativePlaytime >= this.MIN_SCROBBLE_DURATION;
			const minPlayTime = mediaItem.duration * this.MIN_SCROBBLE_PERCENTAGE * 1000;
			const moreThan50Percent = this.cumulativePlaytime >= minPlayTime;

			if (longerThan4min || moreThan50Percent) {
				hasScrobbledCurrent = true;
				onScrobble(mediaItem, this.trace.err.withContext("onScrobble"));
				return true;
			}
			return false;
		};

		MediaItem.onMediaTransition(unloads, (mediaItem) => {
			// What the open segment has counted for the track we are leaving, if it is that track's. It is
			// counted from here on: the segment goes on for whichever track it was played on.
			if (this.playStartMono !== undefined && this.countsForCurrent(this.segmentProduct)) {
				const now = performance.now();
				this.cumulativePlaytime += now - this.playStartMono;
				this.playStartMono = now;
			}
			// Scrobble the track we just finished (works for normal transitions AND repeat mode)
			if (this.currentMediaItem !== undefined) {
				checkAndScrobble(this.currentMediaItem);
			}
			const left = this.currentMediaItem === undefined ? undefined : String(this.currentMediaItem.id);
			const announced = String(mediaItem.id);
			this.currentMediaItem = mediaItem;
			// The announced track starts from what was played on it before its announcement. With no track
			// announced before, nothing could have been counted for another one: the count is this one's.
			this.cumulativePlaytime = this.otherPlaytime.get(announced) ?? (left === undefined ? this.cumulativePlaytime : 0);
			for (const product of this.otherPlaytime.keys()) {
				if (product !== this.segmentProduct) this.otherPlaytime.delete(product);
			}
			this.otherPlaytime.delete(announced);
			// A segment still open on the track just left counts for nothing now: that track's count is
			// settled, and the pause that closes this segment can arrive after this announcement.
			if (left !== undefined && left !== announced && this.segmentProduct === left) {
				delete this.lastPlayStart;
				delete this.playStartMono;
				delete this.segmentProduct;
			}
			hasScrobbledCurrent = false;
		});

		// Handle playback ending without transition (end of queue)
		redux.intercept("playbackControls/SET_PLAYBACK_STATE", unloads, (state) => {
			if (state === "IDLE" && this.currentMediaItem !== undefined) {
				checkAndScrobble(this.currentMediaItem);
			}
		});
	});

	/** Triggered on "playbackControls/SET_PLAYBACK_STATE" */
	public static onState: AddReceiver<redux.PlaybackState> = registerEmitter((onState) =>
		redux.intercept("playbackControls/SET_PLAYBACK_STATE", unloads, (playbackState) =>
			onState(playbackState, this.trace.err.withContext("onState")),
		),
	);
}
