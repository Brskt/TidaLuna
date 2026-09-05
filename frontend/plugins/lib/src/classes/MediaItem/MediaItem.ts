import { asyncDebounce, memoize, memoizeArgless, registerEmitter, type AddReceiver, type Emit } from "@inrixia/helpers";
import type { IRecording, ITrack } from "musicbrainz-api";

import { ftch, ReactiveStore, type LunaUnload, type LunaUnloads, type Tracer } from "@luna/core";

import { getPlaybackInfo, parseDate, sameItemId, type PlaybackInfo } from "../../helpers";
import { libTrace, unloads } from "../../index.safe";
import * as redux from "../../redux";
import { Album } from "../Album";
import { Artist } from "../Artist";
import { ContentBase, type TCoverOpts } from "../ContentBase";
import { PlayState } from "../PlayState";
import { Quality } from "../Quality";
import { TidalApi } from "../TidalApi";
import { download, downloadProgress } from "./MediaItem.download.native";
import { availableTags, makeTags, MetaTags } from "./MediaItem.tags";
import { getStreamBytes, parseStreamFormat } from "./parseStreamFormat.native";

export type MediaFormat = {
	bitDepth?: number;
	sampleRate?: number;
	codec?: string;
	duration?: number;
	bytes?: number;
	bitrate?: number;
};
type MediaItemCache = {
	format?: { [K in redux.AudioQuality]?: MediaFormat };
	actualAudioQuality?: redux.AudioQuality;
};

export class MediaItem extends ContentBase {
	public static readonly trace: Tracer = libTrace.withSource(".MediaItem").trace;
	public static readonly availableTags = availableTags;

	private static cache = ReactiveStore.getStore("@luna/MediaItemCache", 512);

	private static _supportsSpatialAudio: boolean | undefined;
	public static get supportsSpatialAudio(): boolean | undefined {
		return MediaItem._supportsSpatialAudio;
	}

	private static async fetchMediaItem(itemId: redux.ItemId, contentType: redux.ContentType) {
		// Suppress missing content warning when programmatically loading mediaItems
		const clearWarnCatch = redux.intercept("message/MESSAGE_WARN", unloads, (message) => {
			if (message?.message === "The content is no longer available") return true;
		});
		try {
			// Taken FIRST when it exists, because naming a replacement is TIDAL saying this id is
			// no longer served: asking for it anyway buys a guaranteed deadline and then a
			// refused fetch. ONE hop, never a chain: a pair of stubs naming each other is a
			// shape this cannot rule out, and a loop costs more than a missing tag.
			const standIn = redux.replacementTrackId(redux.store.getState(), itemId);
			if (standIn !== undefined) {
				const served = await this.loadMediaItem(standIn, contentType);
				if (served !== undefined) return served;
			}
			return await this.loadMediaItem(itemId, contentType);
		} finally {
			clearWarnCatch();
		}
	}

	/** Resolve exactly the id asked for, from the store if TIDAL has it and the network if not. */
	private static async loadMediaItem(itemId: redux.ItemId, contentType: redux.ContentType) {
		// Asked for, then read out of the store, NOT waited for as an action. TIDAL answers
		// this load from a saga, and a saga's `put` cannot reach an interceptor hung off the
		// `dispatch` property (see `awaitStoreValue`); waiting on
		// `LOAD_SINGLE_MEDIA_ITEM_SUCCESS` spent the whole timeout on every call, and did it
		// holding a lock shared by every other lookup, which is what made a list of misses
		// resolve one item per timeout. The load itself was never the problem: it runs, and
		// its own reducer writes the item here.
		// Through the same gate as a direct fetch: this dispatch makes TIDAL fetch on our
		// behalf: it spends from the same budget rather than slipping past it.
		await TidalApi.rateGate.pass();
		redux.actions["content/LOAD_SINGLE_MEDIA_ITEM"]({
			id: itemId,
			itemType: contentType,
		});
		const mediaItem = await redux.awaitStoreValue((state) => state.content.mediaItems[itemId as keyof redux.Content["mediaItems"]], unloads);
		if (mediaItem !== undefined) return mediaItem;

		// The deadline passed with nothing written, which a track unavailable in this
		// account's region reaches on every call. Only a track has a single-item endpoint to
		// ask directly; a video has none and has nowhere else to go.
		if (contentType !== "track") return undefined;
		const track = await TidalApi.track(itemId);
		if (track === undefined) return undefined;
		return { type: contentType, item: track } as redux.MediaItem;
	}

	// #region Static Construction
	public static async fromId(itemId?: redux.ItemId, contentType: redux.ContentType = "track"): Promise<MediaItem | undefined> {
		if (itemId === undefined) return;
		const item = await super.fromStore(itemId, "mediaItems", async (mediaItem) => {
			mediaItem = mediaItem ??= await this.fetchMediaItem(itemId, contentType);
			if (mediaItem === undefined) return;
			// Create the reactive cache entry only once the item is known to exist, so unavailable
			// ids never leak one; the reactive cache bounds and disposes it independently.
			const cache = await MediaItem.cache.getReactive<MediaItemCache>(String(itemId), { format: {} });
			return new MediaItem(itemId, mediaItem, contentType, cache);
		});
		// Fetch real quality for spatial tracks (only for tracks, not videos)
		if (item && contentType === "track") await item.fetchBestQuality();
		return item;
	}
	public static fromIsrc: (isrc: string) => Promise<MediaItem | undefined> = memoize(async (isrc) => {
		let bestMediaItem: MediaItem | undefined = undefined;
		for await (const track of TidalApi.isrc(isrc)) {
			// If quality is higher than current best, set as best
			const maxTrackQuality = Quality.max(...Quality.fromMetaTags(track.attributes.mediaTags as redux.MediaMetadataTag[]));
			if (maxTrackQuality > (bestMediaItem?.bestQuality ?? Quality.Lowest)) {
				bestMediaItem = (await MediaItem.fromId(track.id)) ?? bestMediaItem;
				if ((bestMediaItem?.bestQuality ?? Quality.Lowest) >= Quality.Max) return bestMediaItem;
			}
		}
		return bestMediaItem;
	});
	public static async fromPlaybackContext(playbackContext?: redux.PlaybackContext) {
		// This has to be here to avoid cyclic requirements breaking
		playbackContext ??= PlayState.playbackContext;
		if (playbackContext?.actualProductId === undefined) return undefined;
		const mediaItem = await this.fromId(playbackContext.actualProductId, playbackContext.actualVideoQuality === null ? "track" : "video");
		// mediaItem?.setFormatAttrs({
		// 	bitDepth: playbackContext.bitDepth ?? undefined,
		// 	sampleRate: playbackContext.sampleRate ?? undefined,
		// 	duration: playbackContext.actualDuration ?? undefined,
		// 	codec: playbackContext.codec ?? undefined,
		// });
		return mediaItem;
	}
	public static async *fromIds(ids?: (redux.ItemId | undefined)[]) {
		if (ids === undefined) return;
		for (const itemId of ids.filter((id) => id !== undefined)) {
			const mediaItem = await MediaItem.fromId(itemId);
			if (mediaItem !== undefined) yield mediaItem;
		}
	}
	public static async *fromTMediaItems(tMediaItems?: ({ item: { id: redux.ItemId }; type: redux.ContentType } | undefined)[]) {
		if (tMediaItems === undefined) return;
		for (const tMediaItem of tMediaItems.filter((tMediaItem) => tMediaItem !== undefined)) {
			const mediaItem = await MediaItem.fromId(tMediaItem.item.id, tMediaItem.type);
			if (mediaItem !== undefined) yield mediaItem;
		}
	}

	// #region Listeners
	/** Triggered on "player/PRELOAD_ITEM" */
	public static onPreload: AddReceiver<MediaItem> = registerEmitter((emit) =>
		redux.intercept("player/PRELOAD_ITEM", unloads, async (item) => {
			if (item?.productId === undefined) return MediaItem.trace.warn("player/PRELOAD_ITEM intercepted without productId!", item);
			const mediaItem = await this.fromId(item.productId, item.productType);
			if (mediaItem === undefined) return;
			emit(mediaItem, mediaItem.trace.err.withContext("preloadItem.runListeners"));
		}),
	);
	/** Triggered on "playbackControls/MEDIA_PRODUCT_TRANSITION"*/
	public static onMediaTransition: AddReceiver<MediaItem> = registerEmitter((emit) =>
		redux.intercept(
			"playbackControls/MEDIA_PRODUCT_TRANSITION",
			unloads,
			asyncDebounce(async ({ playbackContext }: redux.InterceptPayload<"playbackControls/MEDIA_PRODUCT_TRANSITION">) => {
				const mediaItem = await this.fromPlaybackContext(playbackContext);
				if (mediaItem === undefined) return;

				// Detect spatial audio support based on actual playback
				if (mediaItem.tidalItem.audioModes?.includes("DOLBY_ATMOS") || mediaItem.tidalItem.audioModes?.includes("SONY_360RA")) {
					this._supportsSpatialAudio = playbackContext.actualAudioMode !== "STEREO";
				}

				await emit(mediaItem, mediaItem.trace.err.withContext("mediaProductTransition.runListeners"));
			}),
		),
	);
	/**
	 * Triggered on "playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION"
	 * Warning! Not always called, **dont rely on this over onMediaTransition**
	 * */
	public static onPreMediaTransition: AddReceiver<MediaItem> = registerEmitter((emit) =>
		redux.intercept(
			"playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION",
			unloads,
			asyncDebounce(
				async ({ mediaProduct: { productId, productType } }: redux.InterceptPayload<"playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION">) => {
					const mediaItem = await this.fromId(productId, productType);
					if (mediaItem === undefined) return;
					await emit(mediaItem, mediaItem.trace.err.withContext("prefillMPT.runListeners"));
				},
			),
		),
	);
	// #endregion
	public readonly tidalItem: Readonly<redux.Track>;
	public readonly trace: Tracer;

	constructor(
		public readonly id: redux.ItemId,
		tidalMediaItem: redux.MediaItem,
		public readonly contentType: redux.ContentType,
		private readonly cache: MediaItemCache,
	) {
		super();
		// Ick, really need to figure out how to deal with videos
		this.tidalItem = tidalMediaItem?.item as redux.Track;
		if (this.tidalItem === undefined) MediaItem.trace.err.withContext("MediaItem constructor", this).throw("Tidal media item is undefined!");
		this.trace = MediaItem.trace.withSource(`[${this.tidalItem.title ?? id}]`).trace;
	}

	/**
	 * The id to ASK the API with, which is not always the id this row is called by.
	 *
	 * `id` names the row: what the playlist holds, what caches key on, what the DOM is tagged
	 * with, and it must never move. But a track TIDAL drops from the catalogue leaves that id
	 * naming a resource it will not serve and hands back a replacement, so every later request
	 * built from `id` is answered with a 403 for a row the listener can play. Taken off the item
	 * actually served rather than tracked beside it, the two cannot drift apart.
	 */
	public get servedId(): redux.ItemId {
		return this.tidalItem.id ?? this.id;
	}

	/** True for `id` and for the id TIDAL serves under it, which a replaced row needs both of. */
	public answersTo(itemId: redux.ItemId | undefined): boolean {
		return sameItemId(itemId, this.id) || sameItemId(itemId, this.servedId);
	}

	public play() {
		return PlayState.play(this.id);
	}

	/**
	 * Fetches the Tidal media item from the API to ensure properties like `bpm` are populated.
	 * Is idempotent so can be called multiple times without causing re-fetch.
	 */
	public fetchTidalMediaItem: () => Promise<void> = memoizeArgless(async () => {
		const tidalItem = await TidalApi.track(this.servedId);
		if (tidalItem !== undefined) (<redux.Track>this.tidalItem) = tidalItem;
	});

	// #region MusicBrainz
	public brainzItem: () => Promise<ITrack | undefined> = memoize(async () => {
		const releaseTrackFromRecording = async (recording: IRecording) => {
			// If a recording exists then fetch the full recording details including media for title resolution
			const release = await ftch
				.json<IRecording>(`https://musicbrainz.org/ws/2/recording/${recording.id}?inc=releases+media+artist-credits+isrcs&fmt=json`)
				.then(({ releases }) => releases?.filter((release) => release["text-representation"].language === "eng")[0] ?? releases?.[0])
				.catch(this.trace.warn.withContext("brainzItem.getISRCRecordings"));
			if (release === undefined) return undefined;

			const releaseTrack = release.media?.[0].tracks?.[0];
			releaseTrack.recording ??= recording;
			return releaseTrack;
		};

		if (this.tidalItem.isrc !== undefined) {
			// Lookup the recording from MusicBrainz by ISRC
			const recording = await ftch
				.json<{ recordings: IRecording[] }>(`https://musicbrainz.org/ws/2/isrc/${this.tidalItem.isrc}?inc=isrcs&fmt=json`)
				.then(({ recordings }) => recordings[0])
				.catch((err) => {
					if (err.message !== "Status code is 404") this.trace.warn.withContext("brainzItem.getISRCRecordings");
				});

			if (recording !== undefined) return releaseTrackFromRecording(recording);
		}

		const album = await this.album();
		const albumRelease = await album?.brainzRelease();
		if (albumRelease === undefined) return;

		const volumeNumber = (this.tidalItem.volumeNumber ?? 1) - 1;
		const trackNumber = (this.tidalItem.trackNumber ?? 1) - 1;

		let brainzItem = albumRelease?.media?.[volumeNumber]?.tracks?.[trackNumber];
		// If this is not the english version of the release try to find the english version of the release track
		if (albumRelease?.["text-representation"].language !== "eng" && brainzItem?.recording !== undefined) {
			return (await releaseTrackFromRecording(brainzItem.recording)) ?? brainzItem;
		}
		return brainzItem;
	});
	public brainzId: () => Promise<string | undefined> = memoize(async () => {
		const brainzItem = await this.brainzItem();
		return brainzItem?.recording.id;
	});
	// #endregion

	// #region Async properties
	public album: () => Promise<Album | undefined> = memoize(async () => {
		if (this.tidalItem.album?.id) return Album.fromId(this.tidalItem.album?.id);
	});

	public artist: () => Promise<Artist | undefined> = memoize(async () => {
		if (this.tidalItem.artist?.id) return Artist.fromId(this.tidalItem.artist.id);
		if (this.tidalItem.artists?.[0]?.id) return Artist.fromId(this.tidalItem.artists?.[0].id);
		return (await this.album())?.artist();
	});

	public artists: () => Promise<Promise<Artist | undefined>[]> = memoize(async () => {
		if (this.tidalItem.artists) return this.tidalItem.artists.map((artist) => Artist.fromId(artist.id));
		return (await this.album())?.artists() ?? [];
	});

	public async *isrcs(): AsyncIterable<string> {
		if (this.contentType !== "track") return;
		const seen = new Set<string>();
		if (this.tidalItem.isrc) {
			yield this.tidalItem.isrc;
			seen.add(this.tidalItem.isrc);
		}

		const brainzItem = await this.brainzItem();
		if (brainzItem?.recording.isrcs) {
			for (const isrc of brainzItem.recording.isrcs) {
				if (seen.has(isrc)) continue;
				yield isrc;
				seen.add(isrc);
			}
		}
	}

	public isrc: () => Promise<string | undefined> = memoize(async () => {
		for await (const isrc of this.isrcs()) return isrc;
	});

	public lyrics: () => Promise<redux.Lyrics | undefined> = memoize(() => TidalApi.lyrics(this.servedId));

	public title: () => Promise<string> = memoize(async () => {
		const brainzItem = await this.brainzItem();
		return ContentBase.formatTitle(this.tidalItem.title, this.tidalItem.version ?? undefined, brainzItem?.title, brainzItem?.["artist-credit"]);
	});

	public releaseDate: () => Promise<Date | undefined> = memoize(async () => {
		let releaseDate = parseDate(this.tidalItem.releaseDate);
		if (releaseDate === undefined) {
			const brainzItem = await this.brainzItem();
			releaseDate = parseDate(brainzItem?.recording?.["first-release-date"]);
		}
		if (releaseDate === undefined) {
			const album = await this.album();
			releaseDate = parseDate(album?.releaseDate);
			if (releaseDate === undefined) {
				const brainzAlbum = await album?.brainzAlbum();
				releaseDate ??= parseDate(brainzAlbum?.date);
			}
		}
		return releaseDate ?? parseDate(this.tidalItem.streamStartDate);
	});

	/**
	 * "year-month-day"
	 */
	public releaseDateStr: () => Promise<string | undefined> = memoize(async () => {
		return (await this.releaseDate())?.toISOString().slice(0, 10);
	});

	public coverUrl: (opts?: TCoverOpts) => Promise<string | undefined> = memoize(async (opts) => {
		const coverUrl = ContentBase.getAlbumCoverUrl(this.tidalItem.album, opts);
		if (coverUrl) return coverUrl;
		const album = await this.album();
		return album?.coverUrl(opts);
	});

	public flacTags: () => Promise<MetaTags> = memoize(() => makeTags(this));

	public async copyright(): Promise<string | undefined> {
		if (!this.tidalItem.copyright) await this.fetchTidalMediaItem();
		return this.tidalItem.copyright ?? undefined;
	}
	public async bpm(): Promise<number | undefined> {
		if (!this.tidalItem.bpm) await this.fetchTidalMediaItem();
		return this.tidalItem.bpm ?? undefined;
	}
	// #endregion

	// #region Properties
	public get trackNumber() {
		return this.tidalItem.trackNumber;
	}
	public get volumeNumber() {
		return this.tidalItem.volumeNumber;
	}
	public get replayGainPeak() {
		return this.tidalItem.peak;
	}
	public get replayGain(): number {
		if (this.contentType !== "track") return 0;
		return this.tidalItem.replayGain;
	}
	public get url(): string {
		return this.tidalItem.url;
	}
	public get qualityTags(): Quality[] {
		if (this.contentType !== "track") return [];
		const tags = Quality.fromMetaTags(this.tidalItem.mediaMetadata?.tags);
		const audioQuality = Quality.fromAudioQuality(this.tidalItem.audioQuality);
		// Placeholder for lossy tracks so TidalTags can display Low/Lowest
		if (tags.length === 0 && audioQuality !== undefined && audioQuality < Quality.High) tags.push(Quality.High);
		return tags;
	}
	public get bestQuality(): Quality {
		if (this.contentType !== "track") {
			this.trace.warn("MediaItem quality called on non-track!", this);
			return Quality.High;
		}
		const allTags = Quality.fromMetaTags(this.tidalItem.mediaMetadata?.tags);
		const hasSpatial = allTags.some((q) => q === Quality.Atmos || q === Quality.Sony630);
		// Filter out spatial audio tags - they can't display correct metadata
		const tags = allTags.filter((q) => q !== Quality.Atmos && q !== Quality.Sony630);
		// For spatial-only tracks, use cached actualAudioQuality or default to HiRes
		if (hasSpatial && tags.length === 0) {
			if (this.cache.actualAudioQuality) {
				return Quality.fromAudioQuality(this.cache.actualAudioQuality) ?? Quality.HiRes;
			}
			return Quality.HiRes;
		}
		return Quality.max(...tags, Quality.fromAudioQuality(this.tidalItem.audioQuality) ?? Quality.Lowest);
	}
	/** Fetches playbackInfo to get real quality for spatial-only tracks */
	public fetchBestQuality: () => Promise<Quality> = memoize(async () => {
		const allTags = Quality.fromMetaTags(this.tidalItem.mediaMetadata?.tags);
		const hasSpatial = allTags.some((q) => q === Quality.Atmos || q === Quality.Sony630);
		const nonSpatialTags = allTags.filter((q) => q !== Quality.Atmos && q !== Quality.Sony630);
		// Spatial-only tracks need playbackInfo lookup to get real quality
		if (hasSpatial && nonSpatialTags.length === 0 && !this.cache.actualAudioQuality) {
			await this.playbackInfo();
		}
		return this.bestQuality;
	});
	public get duration(): number | undefined {
		return this.tidalItem.duration;
	}
	// #endregion

	// #region Max
	public max: () => Promise<MediaItem | undefined> = memoize(async () => {
		if (this.bestQuality >= Quality.Max) return;

		let bestMediaItem: MediaItem = this;
		for await (const isrc of this.isrcs()) {
			const mediaItem = await MediaItem.fromIsrc(isrc);
			if (mediaItem && mediaItem?.bestQuality > bestMediaItem.bestQuality) {
				bestMediaItem = mediaItem;
				if (bestMediaItem.bestQuality >= Quality.Max) break;
			}
		}

		// Dont return self
		if (bestMediaItem.id === this.id) return undefined;
		return bestMediaItem;
	});
	// #endregion

	// #region PlaybackInfo
	public playbackInfo: (audioQuality?: redux.AudioQuality) => Promise<PlaybackInfo | undefined> = memoize(
		async (audioQuality?: redux.AudioQuality) => {
			audioQuality ??= Quality.Max.audioQuality;
			const playbackInfo = await getPlaybackInfo(this.servedId, audioQuality);
			if (!playbackInfo) return undefined;
			const [_, emitFormat] = this.formatEmitters[audioQuality] ?? [];
			this.cache.format ??= {};
			this.cache.format[audioQuality] = {
				...this.cache.format[audioQuality],
				bitDepth: playbackInfo.bitDepth,
				sampleRate: playbackInfo.sampleRate,
			};
			this.cache.actualAudioQuality = playbackInfo.audioQuality;
			emitFormat?.(this.cache.format[audioQuality]!, this.trace.err.withContext("playbackInfo.emitFormat"));
			return playbackInfo;
		},
	);
	// #endregion

	// #region Download
	public async downloadProgress() {
		return downloadProgress(this.id);
	}
	public download: (path: string | string[], audioQuality?: redux.AudioQuality) => Promise<void> = asyncDebounce(
		async (path: string | string[], audioQuality?: redux.AudioQuality) => {
			const playbackInfo = await this.playbackInfo(audioQuality);
			if (!playbackInfo) throw new Error(`Track ${this.id} is not available`);
			// Only BTS carries tags: the DASH branch of download() has none to write, and flacTags()
			// is a burst of MusicBrainz and Tidal lookups. Awaited after playbackInfo rather than
			// beside it because the manifest type is what says whether the tags are wanted at all.
			const tags = playbackInfo.manifestMimeType === "application/vnd.tidal.bts" ? await this.flacTags() : undefined;
			return download(playbackInfo, path, tags);
		},
	);
	public async fileExtension(audioQuality?: redux.AudioQuality): Promise<string> {
		const playbackInfo = await this.playbackInfo(audioQuality);
		if (!playbackInfo) throw new Error(`Track ${this.id} is not available`);
		switch (playbackInfo.manifestMimeType) {
			case "application/dash+xml":
				return "m4a";
			case "application/vnd.tidal.bts":
				return "flac";
		}
	}
	// #endregion

	// #region Format
	private readonly formatEmitters: {
		[K in redux.AudioQuality]?: [onEvent: AddReceiver<MediaFormat>, emitEvent: Emit<MediaFormat>];
	} = {};
	public withFormat(unloads: LunaUnloads, audioQuality: redux.AudioQuality, listener: (format: MediaFormat) => void): LunaUnload {
		const [onFormat] = (this.formatEmitters[audioQuality] ??= registerEmitter<MediaFormat>());
		// Pin this instance while a subscriber is attached so eviction can never orphan the emitter
		// (a re-fetched instance would have empty formatEmitters). Unpinned exactly once, on the
		// subscription's unload (manual call or plugin teardown via unloads).
		MediaItem.pinInstance("mediaItems", this.id);
		const inner = onFormat(unloads, listener);
		let unpinned = false;
		const unpin: LunaUnload = () => {
			if (unpinned) return;
			unpinned = true;
			MediaItem.unpinInstance("mediaItems", this.id);
			// Self-remove from the set (mirrors registerEmitter's own unload) so a manual unsubscribe
			// before plugin teardown doesn't leave this instance-capturing closure retained in the set.
			unloads.delete(unpin);
		};
		unloads.add(unpin);
		// Use actualAudioQuality as fallback key for cache lookup (handles Atmos/Sony360 fallback quality)
		const cacheKey = audioQuality ?? this.cache.actualAudioQuality;
		const cachedFormat = cacheKey !== undefined ? this.cache.format?.[cacheKey] : undefined;
		if (cachedFormat !== undefined) {
			listener(cachedFormat);
			// Trigger updateFormat to complete missing fields (e.g., bitrate from bridge bytes).
			// Safe from infinite loops: updateFormat's anti-spam guard early-returns for
			// non-playing tracks that already have sampleRate.
			if (cachedFormat.bitrate === undefined) {
				this.updateFormat(audioQuality).catch(() => {});
			}
		} else {
			// No cached format - trigger updateFormat so the bridge fallback can populate it
			this.updateFormat(audioQuality).catch(() => {});
		}
		return () => {
			inner();
			unpin();
		};
	}
	public updateFormat: (audioQuality?: redux.AudioQuality, force?: true) => Promise<MediaFormat | undefined> = asyncDebounce(
		async (audioQuality, force) => {
			this.cache.format ??= {};
			const requestedQuality = audioQuality;
			audioQuality ??= Quality.Max.audioQuality;
			let format = (this.cache.format[audioQuality] ??= {});

			if (format.bitrate !== undefined && format.sampleRate !== undefined && force !== true) {
				return format;
			}
			// If we already have sampleRate + bytes but still no bitrate, compute it now and return.
			// If we have sampleRate but no bytes, only retry for the current track (HEAD may have failed).
			if (format.sampleRate !== undefined && force !== true) {
				if (format.bytes !== undefined && format.duration) {
					format.bitrate = (format.bytes / format.duration) * 8;
					return format;
				}
				// `answersTo`, because what plays under a replaced row is the id TIDAL served, not
				// the one the row is called by: compared against `id` alone, such a row never
				// recognises itself as the current track and stops retrying exactly when it could
				// have succeeded.
				const isCurrentTrack = this.answersTo((window as any).__LUNAR_CURRENT_PRODUCT_ID__ ?? PlayState.playbackContext?.actualProductId);
				if (!isCurrentTrack && format.codec !== undefined) return format;
			}

			const playbackInfo = await this.playbackInfo(audioQuality);
			// Re-read format from cache: playbackInfo() replaces the cache entry with a new object,
			// and the local `format` reference captured above would be stale.
			format = this.cache.format[audioQuality] ??= {};
			if (!playbackInfo) {
				// TidaLunar fallback: desktop.tidal.com/v1/playbackinfo returns 403 with web tokens.
				// Use format data from the Rust player bridge (mediaformat event), only valid
				// for the currently playing track (the bridge emits one global mediaformat per load).
				const currentProductId = (window as any).__LUNAR_CURRENT_PRODUCT_ID__ ?? PlayState.playbackContext?.actualProductId;
				if (currentProductId !== undefined && !this.answersTo(currentProductId)) return undefined;
				let bf = (window as any).__LUNAR_MEDIA_FORMAT__;
				if (!bf?.sampleRate) {
					// Bridge data not yet available (new track just loaded): wait up to 5s
					const waited = await Promise.race([
						(window as any).__LUNAR_AWAIT_MEDIA_FORMAT__?.(),
						new Promise<null>((r) => setTimeout(() => r(null), 5000)),
					]);
					bf = waited ?? (window as any).__LUNAR_MEDIA_FORMAT__;
					if (!bf?.sampleRate) return undefined;
				}
				format.sampleRate = bf.sampleRate;
				format.bitDepth = bf.bitDepth || undefined;
				format.codec = bf.codec?.toLowerCase();
				format.bytes = bf.bytes || undefined;
				format.duration = this.duration;
				if (format.bytes && format.duration) format.bitrate = (format.bytes / format.duration) * 8;
				// Emit to all registered audioQuality keys, to notify every subscriber
				for (const key of Object.keys(this.formatEmitters) as redux.AudioQuality[]) {
					const [_, emit] = this.formatEmitters[key]!;
					emit(format, this.trace.err.withContext("updateFormat.bridgeFallback"));
					this.cache.format![key] = format;
				}
				return format;
			}
			format.duration = this.duration;

			if (format.bitDepth === undefined || format.sampleRate === undefined || format.duration === undefined || format.bytes === undefined) {
				const { format: streamFormat, bytes } = await parseStreamFormat(playbackInfo);
				format.bytes = bytes ?? (await getStreamBytes(playbackInfo));
				format.bitDepth = streamFormat.bitsPerSample || format.bitDepth;
				format.sampleRate = streamFormat.sampleRate || format.sampleRate;
				format.duration = streamFormat.duration ?? format.duration;
				format.codec = streamFormat.codec?.toLowerCase() ?? format.codec;
				// Complement with DASH manifest data if available
				if (playbackInfo.manifestMimeType === "application/dash+xml") {
					format.bitrate = playbackInfo.manifest.bandwidth ?? format.bitrate;
					format.sampleRate = playbackInfo.manifest.sampleRate ?? format.sampleRate;
				}
			} else {
				format.bytes = (await getStreamBytes(playbackInfo)) ?? format.bytes;
			}

			// For BTS (FLAC), bytes are only available from the Rust player bridge for the currently playing track.
			// The bridge data may not have arrived yet (reset on load): wait up to 5s if needed.
			if (format.bytes === undefined && this.answersTo((window as any).__LUNAR_CURRENT_PRODUCT_ID__ ?? PlayState.playbackContext?.actualProductId)) {
				let bf = (window as any).__LUNAR_MEDIA_FORMAT__;
				if (!bf?.bytes) {
					const waited = await Promise.race([
						(window as any).__LUNAR_AWAIT_MEDIA_FORMAT__?.(),
						new Promise<null>((r) => setTimeout(() => r(null), 5000)),
					]);
					bf = waited ?? (window as any).__LUNAR_MEDIA_FORMAT__;
				}
				if (bf?.bytes) format.bytes = bf.bytes;
			}

			format.bitrate ??= !!format.bytes && !!format.duration ? (format.bytes / format.duration) * 8 : undefined;

			// Also store format under actual audio quality for cache lookup (handles Atmos/Sony360 fallback)
			if (playbackInfo.audioQuality !== audioQuality) {
				this.cache.format[playbackInfo.audioQuality] = format;
			}

			const [_, emitFormat] = this.formatEmitters[playbackInfo.audioQuality] ?? [];
			emitFormat?.(format, this.trace.err.withContext("updateFormat.emitFormat"));

			// Emit to originally requested quality if different
			if (requestedQuality && requestedQuality !== playbackInfo.audioQuality) {
				const [_, emitRequested] = this.formatEmitters[requestedQuality] ?? [];
				emitRequested?.(format, this.trace.err.withContext("updateFormat.emitFormat.requested"));
			}

			return format;
		},
	);
	// #endregion
}
