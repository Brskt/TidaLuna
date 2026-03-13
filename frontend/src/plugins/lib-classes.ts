// --- @luna/lib runtime classes: MediaItem, PlayState, Quality, registerEmitter ---
// Functional implementations backed by Redux intercept.

import type { ReduxState } from "./redux-patch";

// --- Event emitter infrastructure (matches @inrixia/helpers pattern) ---

type LunaUnloads = Set<() => void>;
type LunaUnload = () => void;
type Emit<T> = (value: T, onError?: (err: unknown) => void) => Promise<void>;
export type AddReceiver<T> = (unloads: LunaUnloads, listener: (value: T) => void | Promise<void>) => LunaUnload;

export function registerEmitter<T>(init: (emit: Emit<T>) => void | LunaUnload): AddReceiver<T> {
    const listeners = new Set<(value: T) => void | Promise<void>>();
    const emit: Emit<T> = async (value, onError) => {
        for (const listener of listeners) {
            try {
                await listener(value);
            } catch (err) {
                if (onError) onError(err);
                else console.error("[luna:emitter]", err);
            }
        }
    };
    init(emit);
    return (unloads: LunaUnloads, listener: (value: T) => void | Promise<void>): LunaUnload => {
        listeners.add(listener);
        const unload = () => listeners.delete(listener);
        unloads.add(unload);
        return unload;
    };
}

// --- Quality (matches upstream Quality class exactly) ---

export class Quality {
    private static readonly idxLookup: Record<number, Quality> = {};
    private constructor(
        private readonly idx: number,
        public readonly name: string,
        public readonly color: string,
    ) {
        Quality.idxLookup[idx] = this;
    }

    public static readonly HiRes = new Quality(6, "HiRes", "#ffd432");
    public static readonly MQA = new Quality(5, "MQA", "#F9BA7A");
    public static readonly Atmos = new Quality(4, "Atmos", "#6ab5ff");
    public static readonly Sony630 = new Quality(3, "Sony630", "#6ab5ff");
    public static readonly High = new Quality(2, "High", "#33FFEE");
    public static readonly Low = new Quality(1, "Low", "#b9b9b9");
    public static readonly Lowest = new Quality(0, "Lowest", "#b9b9b9");
    public static readonly Max = Quality.HiRes;

    public static readonly lookups = {
        metadataTags: {
            HIRES_LOSSLESS: Quality.HiRes,
            [Quality.HiRes.idx]: "HIRES_LOSSLESS",
            MQA: Quality.MQA,
            [Quality.MQA.idx]: "MQA",
            DOLBY_ATMOS: Quality.Atmos,
            [Quality.Atmos.idx]: "DOLBY_ATMOS",
            SONY_360RA: Quality.Sony630,
            [Quality.Sony630.idx]: "SONY_360RA",
            LOSSLESS: Quality.High,
            [Quality.High.idx]: "LOSSLESS",
        } as Record<string | number, string | Quality>,
        audioQuality: {
            HI_RES_LOSSLESS: Quality.HiRes,
            [Quality.HiRes.idx]: "HI_RES_LOSSLESS",
            HI_RES: Quality.MQA,
            [Quality.MQA.idx]: "HI_RES",
            LOSSLESS: Quality.High,
            [Quality.High.idx]: "LOSSLESS",
            HIGH: Quality.Low,
            [Quality.Low.idx]: "HIGH",
            LOW: Quality.Lowest,
            [Quality.Lowest.idx]: "LOW",
        } as Record<string | number, string | Quality>,
    };

    private static fromIdx(idx: number): Quality {
        return this.idxLookup[idx] ?? this.Lowest;
    }

    public static fromMetaTags(qualityTags?: string[]): Quality[] {
        if (!qualityTags) return [];
        return qualityTags
            .map((tag) => this.lookups.metadataTags[tag])
            .filter((q): q is Quality => q instanceof Quality);
    }

    public static fromAudioQuality(audioQuality?: string): Quality | undefined {
        if (audioQuality === undefined) return undefined;
        const q = this.lookups.audioQuality[audioQuality];
        return q instanceof Quality ? q : undefined;
    }

    public static min(...qualities: Quality[]): Quality {
        return Quality.fromIdx(Math.min(...qualities.map((q) => q.idx)));
    }

    public static max(...qualities: Quality[]): Quality {
        return Quality.fromIdx(Math.max(...qualities.map((q) => q.idx)));
    }

    public get audioQuality(): string {
        return Quality.lookups.audioQuality[this.idx] as string;
    }

    public get metadataTag(): string {
        return Quality.lookups.metadataTags[this.idx] as string;
    }

    valueOf(): number {
        return this.idx;
    }
}

// --- MediaFormat type ---

interface MediaFormat {
    sampleRate?: number;
    bitDepth?: number;
    bitrate?: number;
    codec?: string;
}

// --- MediaItem ---

export class MediaItem {
    readonly id: string;
    readonly tidalItem: Readonly<any>;
    readonly contentType: string;

    constructor(id: string, tidalItem: any, contentType: string = "track") {
        this.id = id;
        this.tidalItem = Object.freeze(tidalItem ?? {});
        this.contentType = contentType;
    }

    // --- Instance accessors (best-effort from tidalItem) ---

    get trackNumber(): number { return this.tidalItem.trackNumber ?? 0; }
    get volumeNumber(): number { return this.tidalItem.volumeNumber ?? 0; }
    get replayGain(): number { return this.tidalItem.replayGain ?? 0; }
    get replayGainPeak(): number { return this.tidalItem.peak ?? 1; }
    get url(): string { return this.tidalItem.url ?? ""; }
    get duration(): number | undefined { return this.tidalItem.duration; }

    get qualityTags(): Quality[] {
        return Quality.fromMetaTags(this.tidalItem.mediaMetadata?.tags);
    }

    get bestQuality(): Quality {
        const tags = this.qualityTags;
        return tags.length > 0 ? Quality.max(...tags) : Quality.High;
    }

    async title(): Promise<string> {
        return this.tidalItem.title ?? "Unknown";
    }

    async artist(): Promise<{ id: string; name: string } | undefined> {
        const a = this.tidalItem.artist ?? this.tidalItem.artists?.[0];
        if (!a) return undefined;
        return { id: String(a.id), name: a.name ?? "Unknown" };
    }

    async artists(): Promise<Promise<{ id: string; name: string } | undefined>[]> {
        const list = this.tidalItem.artists ?? [];
        return list.map((a: any) =>
            Promise.resolve(a ? { id: String(a.id), name: a.name ?? "Unknown" } : undefined)
        );
    }

    async album(): Promise<{ id: string; tidalAlbum: any; title: () => Promise<string | undefined>; coverUrl: (opts?: any) => string | undefined } | undefined> {
        const a = this.tidalItem.album;
        if (!a) return undefined;
        return {
            id: String(a.id),
            tidalAlbum: a,
            title: async () => a.title,
            coverUrl: (opts?: { width?: number; height?: number }) => {
                if (!a.cover) return undefined;
                const w = opts?.width ?? 320;
                const h = opts?.height ?? 320;
                return `https://resources.tidal.com/images/${a.cover.replace(/-/g, "/")}/${w}x${h}.jpg`;
            },
        };
    }

    async coverUrl(opts?: { width?: number; height?: number }): Promise<string | undefined> {
        const a = await this.album();
        return a?.coverUrl(opts);
    }

    async lyrics(): Promise<any | undefined> { return undefined; }
    async isrc(): Promise<string | undefined> { return this.tidalItem.isrc; }
    async releaseDate(): Promise<Date | undefined> {
        const d = this.tidalItem.streamStartDate ?? this.tidalItem.releaseDate;
        return d ? new Date(d) : undefined;
    }
    async releaseDateStr(): Promise<string | undefined> {
        return this.tidalItem.streamStartDate ?? this.tidalItem.releaseDate;
    }
    async copyright(): Promise<string | undefined> { return this.tidalItem.copyright; }
    async bpm(): Promise<number | undefined> { return undefined; }
    async brainzId(): Promise<string | undefined> { return undefined; }
    async brainzItem(): Promise<any | undefined> { return undefined; }
    async flacTags(): Promise<any> { return {}; }

    async play(): Promise<void> {
        MediaItem._redux?.actions["playbackControls/PLAY"]?.(undefined);
    }

    async playbackInfo(_audioQuality?: string): Promise<any | undefined> {
        return undefined;
    }

    async fetchTidalMediaItem(): Promise<void> {}

    async fetchBestQuality(): Promise<Quality> {
        return this.bestQuality;
    }

    async max(): Promise<MediaItem | undefined> {
        return this;
    }

    async fileExtension(_audioQuality?: string): Promise<string> {
        return "flac";
    }

    // withFormat: provides format info from the Rust player bridge.
    // If format data is already available, calls listener immediately.
    // Otherwise waits for the next mediaformat event from the bridge.
    withFormat(unloads: LunaUnloads, _audioQuality: string, listener: (format: MediaFormat) => void): LunaUnload {
        let cancelled = false;
        const w = window as any;

        const buildFormat = (raw: any): MediaFormat | null => {
            if (!raw || typeof raw !== "object") return null;
            const format: MediaFormat = {
                sampleRate: raw.sampleRate,
                bitDepth: raw.bitDepth,
                codec: raw.codec,
            };
            if (raw.bytes && this.duration) {
                format.bitrate = Math.round((raw.bytes * 8) / this.duration);
            }
            return format;
        };

        const isCurrentTrack = String(w.__LUNAR_CURRENT_PRODUCT_ID__) === String(this.id);

        if (isCurrentTrack) {
            // Try existing format data first
            const existing = buildFormat(w.__LUNAR_MEDIA_FORMAT__);
            if (existing) {
                listener(existing);
            } else {
                // Wait for the bridge to send mediaformat
                const awaiter = w.__LUNAR_AWAIT_MEDIA_FORMAT__;
                if (typeof awaiter === "function") {
                    awaiter().then((data: any) => {
                        if (cancelled) return;
                        const fmt = buildFormat(data);
                        if (fmt) listener(fmt);
                    });
                }
            }
        }

        const unload = () => { cancelled = true; };
        unloads.add(unload);
        return unload;
    }

    async updateFormat(_audioQuality?: string, _force?: boolean): Promise<MediaFormat | undefined> {
        return undefined;
    }

    // --- Static properties ---

    static _redux: ReduxState;
    static onMediaTransition: AddReceiver<MediaItem>;
    static onPreload: AddReceiver<MediaItem>;
    static onPreMediaTransition: AddReceiver<MediaItem>;

    // Can be called with or without arguments.
    // Without args: reads current playback context from Redux state.
    // With args: uses the provided playbackContext.
    static async fromPlaybackContext(playbackContext?: any): Promise<MediaItem | undefined> {
        if (!playbackContext) {
            // No argument: read current context from Redux state
            playbackContext = MediaItem._redux?.store?.getState()?.playbackControls?.playbackContext;
        }
        const productId = playbackContext?.actualProductId;
        if (productId == null) return undefined;

        const state = MediaItem._redux?.store?.getState();
        let tidalItem: any = undefined;

        if (state) {
            tidalItem = state.content?.mediaItems?.[productId]?.item
                ?? state.content?.tracks?.[productId];

            if (!tidalItem) {
                const elements = state.playQueue?.elements ?? state.playQueue?.items ?? [];
                for (const el of elements) {
                    const item = el?.mediaItem?.item ?? el?.item;
                    if (item && String(item.id) === String(productId)) {
                        tidalItem = item;
                        break;
                    }
                }
            }
        }

        if (!tidalItem) tidalItem = { id: productId };
        return new MediaItem(String(productId), tidalItem, playbackContext?.actualContentType ?? "track");
    }

    static async fromId(itemId?: string | number): Promise<MediaItem | undefined> {
        if (itemId == null) return undefined;
        const state = MediaItem._redux?.store?.getState();

        let tidalItem: any = undefined;
        if (state) {
            tidalItem = state.content?.mediaItems?.[itemId]?.item
                ?? state.content?.tracks?.[itemId];

            if (!tidalItem) {
                const elements = state.playQueue?.elements ?? state.playQueue?.items ?? [];
                for (const el of elements) {
                    const item = el?.mediaItem?.item ?? el?.item;
                    if (item && String(item.id) === String(itemId)) {
                        tidalItem = item;
                        break;
                    }
                }
            }
        }

        if (!tidalItem) tidalItem = { id: itemId };
        return new MediaItem(String(itemId), tidalItem);
    }
}

// --- PlayState ---

export class PlayState {
    static _redux: ReduxState;
    static onScrobble: AddReceiver<MediaItem>;
    static onState: AddReceiver<string>;

    static get playbackControls(): any {
        return PlayState._redux?.store?.getState()?.playbackControls ?? {};
    }

    static get playbackContext(): any {
        return PlayState.playbackControls?.playbackContext ?? {};
    }

    static get playQueue(): any {
        return PlayState._redux?.store?.getState()?.playQueue ?? {};
    }

    static get currentTime(): number {
        return PlayState.playbackControls?.currentTime ?? 0;
    }

    static get state(): string {
        return PlayState.playbackControls?.desiredPlaybackState
            ?? PlayState.playbackControls?.playbackState
            ?? "IDLE";
    }

    static get desiredState(): string {
        return PlayState.playbackControls?.desiredPlaybackState ?? "IDLE";
    }

    static get playing(): boolean {
        return PlayState.state === "PLAYING";
    }

    static get shuffle(): boolean {
        return PlayState.playQueue?.shuffled ?? false;
    }

    static get repeatMode(): string {
        return PlayState.playQueue?.repeatMode ?? "REPEAT_OFF";
    }

    static play(mediaItemId?: string | number): void {
        if (mediaItemId != null) {
            PlayState._redux?.actions["playbackControls/PLAY"]?.({ mediaItemId });
        } else {
            PlayState._redux?.actions["playbackControls/PLAY"]?.(undefined);
        }
    }

    static pause(): void {
        PlayState._redux?.actions["playbackControls/PAUSE"]?.(undefined);
    }

    static next(): void {
        PlayState._redux?.actions["playbackControls/SKIP_NEXT"]?.(undefined);
    }

    static previous(): void {
        PlayState._redux?.actions["playbackControls/SKIP_PREVIOUS"]?.(undefined);
    }

    static seek(time: number): void {
        PlayState._redux?.actions["playbackControls/SEEK"]?.(time);
    }

    static setShuffle(shuffle: boolean): void {
        PlayState._redux?.actions["playQueue/SET_SHUFFLE"]?.(shuffle);
    }

    static setRepeatMode(repeatMode: string): void {
        PlayState._redux?.actions["playQueue/SET_REPEAT_MODE"]?.(repeatMode);
    }

    static nextMediaItem(): MediaItem | undefined {
        const q = PlayState.playQueue;
        const idx = (q?.currentIndex ?? -1) + 1;
        const elements = q?.elements ?? q?.items ?? [];
        const el = elements[idx];
        const item = el?.mediaItem?.item ?? el?.item;
        if (!item) return undefined;
        return new MediaItem(String(item.id), item);
    }

    static previousMediaItem(): MediaItem | undefined {
        const q = PlayState.playQueue;
        const idx = (q?.currentIndex ?? 0) - 1;
        if (idx < 0) return undefined;
        const elements = q?.elements ?? q?.items ?? [];
        const el = elements[idx];
        const item = el?.mediaItem?.item ?? el?.item;
        if (!item) return undefined;
        return new MediaItem(String(item.id), item);
    }
}

// --- Initialization: wire event emitters to Redux intercepts ---

export function initLibClasses(redux: ReduxState, unloads: LunaUnloads): void {
    MediaItem._redux = redux;
    PlayState._redux = redux;

    MediaItem.onMediaTransition = registerEmitter<MediaItem>((emit) => {
        redux.intercept("playbackControls/MEDIA_PRODUCT_TRANSITION", unloads, (payload: any) => {
            MediaItem.fromPlaybackContext(payload?.playbackContext ?? payload).then((mediaItem) => {
                if (mediaItem) emit(mediaItem);
            });
        });
    });

    MediaItem.onPreload = registerEmitter<MediaItem>((emit) => {
        redux.intercept("player/PRELOAD_ITEM", unloads, (payload: any) => {
            MediaItem.fromPlaybackContext(payload?.playbackContext ?? payload).then((mediaItem) => {
                if (mediaItem) emit(mediaItem);
            });
        });
    });

    MediaItem.onPreMediaTransition = registerEmitter<MediaItem>((emit) => {
        redux.intercept("playbackControls/PREFILL_MEDIA_PRODUCT_TRANSITION", unloads, (payload: any) => {
            MediaItem.fromPlaybackContext(payload?.playbackContext ?? payload).then((mediaItem) => {
                if (mediaItem) emit(mediaItem);
            });
        });
    });

    PlayState.onState = registerEmitter<string>((emit) => {
        redux.intercept(
            ["playbackControls/SET_PLAYBACK_STATE", "playbackControls/PLAY", "playbackControls/PAUSE"],
            unloads,
            () => { emit(PlayState.state); }
        );
    });

    PlayState.onScrobble = registerEmitter<MediaItem>(() => {
        // Scrobble detection would need playback time tracking.
        // Stub: never fires. Plugins relying on this won't crash.
    });
}
