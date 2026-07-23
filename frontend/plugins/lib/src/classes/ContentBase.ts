import type { MaybePromise } from "@inrixia/helpers";
import type { IArtistCredit } from "musicbrainz-api";

import { BoundedCache } from "@luna/core";

import * as redux from "../redux";
import type { Artist } from "./Artist";

type ContentType = keyof redux.Content;
type ContentItem<K extends ContentType> = redux.Content[K][keyof redux.Content[K]];
type ContentClass<K extends ContentType> = {
	new (itemId: redux.ItemId, contentItem: ContentItem<K>, ...args: any[]): any;
};
export type TCoverRes = "1280" | "640" | "320" | "160" | "80";
export type TCoverType = "video" | "image";
export type TCoverOpts = {
	res?: TCoverRes;
	type?: TCoverType;
	fallback?: false;
};

export class ContentBase {
	// Bounded LRU of content instances keyed by `${contentType}:${itemId}`. Eviction just drops the
	// instance (its memoized state is GC'd); the reactive cache it reads is bounded and disposed
	// separately in ReactiveStore, so eviction here never severs a still-referenced instance.
	// Instances with live subscribers are pinned (see pinInstance) so they are never evicted.
	private static readonly _instances = new BoundedCache<string, ContentBase>(512, (key) => {
		// Gated on the mirrored Rust log level (>= 2, like sendDbgIpc): surfaces evictions in the
		// [JS] logs so the 512 cap is observable, and stays silent (unemitted) in normal use.
		if (Number((window as any).__TIDALUNAR_LOG_LEVEL__ ?? 0) >= 2)
			console.log(`[@luna/lib.ContentBase] evicted "${key}" - _instances at cap ${ContentBase._instances.size}`);
	});

	private static instanceKey(contentType: ContentType, itemId: redux.ItemId): string {
		return `${contentType}:${itemId}`;
	}

	/** Pin an instance so cap eviction skips it while a subscriber holds it. Refcounted. */
	public static pinInstance(contentType: ContentType, itemId: redux.ItemId): void {
		this._instances.pin(this.instanceKey(contentType, itemId));
	}
	public static unpinInstance(contentType: ContentType, itemId: redux.ItemId): void {
		this._instances.unpin(this.instanceKey(contentType, itemId));
	}

	/**
	 * Ensure instances of ContentClass's are properly cached and abstracts fetching from the store.
	 */
	protected static async fromStore<K extends ContentType, C extends ContentClass<K>, I extends InstanceType<C>>(
		itemId: redux.ItemId,
		contentType: K,
		generator: (contentItem?: ContentItem<K>) => MaybePromise<I | undefined>,
	): Promise<I | undefined> {
		const key = this.instanceKey(contentType, itemId);
		const existing = this._instances.get(key);
		if (existing !== undefined) return existing as I;

		const contentClass = await generator(this.getItemFromStore(contentType, itemId));
		if (contentClass === undefined) return;

		// A concurrent fromStore for the same key may have cached an instance during the await;
		// if so return that winner and drop ours (avoids overwriting/disposing a live instance).
		const winner = this._instances.get(key);
		if (winner !== undefined) return winner as I;

		this._instances.set(key, contentClass as ContentBase);
		return contentClass;
	}

	/**
	 * Fetches a content item from redux.store.content
	 */
	public static getItemFromStore<K extends ContentType>(contentType: K, itemId: redux.ItemId): ContentItem<K> {
		const storeContent = redux.store.getState().content;
		return storeContent[contentType][itemId as keyof redux.Content[K]] as ContentItem<K>;
	}

	protected static formatTitle(tidalTitle?: string, tidalVersion?: string, brainzTitle?: string, brainzCredit?: IArtistCredit[]): string {
		brainzTitle = brainzTitle?.replaceAll("’", "'");

		let title = brainzTitle ?? tidalTitle;
		if (title === undefined) throw new Error("Title is undefined");

		// If the title has feat and its validated by musicBrainz then use the tidal title.
		if (tidalTitle?.includes("feat. ") && !brainzTitle?.includes("feat. ")) {
			const mbHasFeat = brainzCredit && brainzCredit.findIndex((credit) => credit.joinphrase === " feat. ") !== -1;
			if (mbHasFeat) title = tidalTitle;
		}

		// Dont use musicBrainz disambiguation as its not the same as the tidal version!
		if (tidalVersion && !title.toLowerCase().includes(tidalVersion.toLowerCase())) title += ` (${tidalVersion})`;

		return title;
	}

	public static getAlbumCoverUrl(album?: redux.Album | null, opts?: TCoverOpts) {
		if (!album) return;
		let type = opts?.type ?? "image";
		if (type === "video" && !album.videoCover && !(opts?.fallback === false)) type = "image";
		const uuid = type === "image" ? album.cover : album.videoCover;
		if (uuid) return ContentBase.formatCoverUrl(uuid, opts);
	}

	public static formatCoverUrl(uuid?: string, opts?: TCoverOpts) {
		if (!uuid) return;
		const type = opts?.type ?? "image";
		const res = opts?.res ?? "1280";
		const ext = type === "image" ? "jpg" : "mp4";
		return `https://resources.tidal.com/${type}s/${uuid.split("-").join("/")}/${res}x${res}.${ext}`;
	}

	public static async artistNames(artists?: Promise<Promise<Artist | undefined>[]> | Promise<Artist | undefined>[]): Promise<string[]> {
		const _artists = await artists;
		if (!_artists) return [];
		const artistNames = [];
		for await (const artist of _artists) {
			if (artist?.name) artistNames.push(artist?.name);
		}
		return artistNames;
	}
}
