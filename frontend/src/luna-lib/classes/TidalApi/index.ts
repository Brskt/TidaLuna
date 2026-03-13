import { type Tracer } from "@luna/core";

import { memoize, memoizeArgless, type Memoized } from "@inrixia/helpers";

import type { TApiTrack } from "./types/Tracks";

import { getCredentials } from "../../helpers";
import { libTrace } from "../../index.safe";
import * as redux from "../../redux";

import { PlaybackInfoResponse } from "./types/PlaybackInfo";

import { invokeIpc } from "../../../ipc";

export type * from "./types";
export type { PlaybackInfoResponse };

export class TidalApi {
	public static trace: Tracer = libTrace.withSource("TidalApi").trace;
	private static unavailableTracks = new Set<redux.ItemId>();

	/**
	 * Send credentials to the Rust backend so it can make authenticated
	 * API calls. Should be called once after initCore() when the Redux
	 * store and captured token are both available.
	 */
	public static async pushCredentialsToRust(): Promise<void> {
		const { clientId, token } = await getCredentials();
		const state = redux.store.getState();
		const countryCode = state?.session?.countryCode ?? "US";
		const locale = state?.settings?.language ?? "en_US";
		await invokeIpc("tidal.set_credentials", token, clientId, countryCode, locale);
	}

	// Kept for plugin backwards compatibility
	public static async getAuthHeaders() {
		const { clientId, token } = await getCredentials();
		return {
			Authorization: `Bearer ${token}`,
			"x-tidal-token": clientId,
		};
	}
	public static readonly queryArgs: Memoized<() => string> = memoizeArgless(() => {
		const store = redux.store.getState();
		return `countryCode=${store.session.countryCode}&deviceType=DESKTOP&locale=${store.settings.language}`;
	});

	// Browser-side fetch matching upstream TidaLuna — same-origin requests
	// to desktop.tidal.com with auth headers. Retries once on transient errors.
	public static fetch = memoize(async <T>(url: string): Promise<T | undefined> => {
		let retry = true;
		while (true) {
			const res = await fetch(url, {
				headers: await this.getAuthHeaders(),
			});
			if (res.status >= 200 && res.status < 300) return res.json();
			if (res.status === 403 || res.status === 404) return undefined;
			if (!retry) throw new Error(`TidalApi.fetch: ${res.status} ${res.statusText} for ${url}`);
			retry = false;
			await new Promise(r => setTimeout(r, 1000));
		}
	});

	public static async track(trackId: redux.ItemId): Promise<redux.Track | undefined> {
		return this.fetch<redux.Track>(`https://desktop.tidal.com/v1/tracks/${trackId}?${this.queryArgs()}`);
	}

	public static async playbackInfo(trackId: redux.ItemId, audioQuality: redux.AudioQuality): Promise<PlaybackInfoResponse | undefined> {
		if (this.unavailableTracks.has(trackId)) return undefined;
		const result = await this.fetch<PlaybackInfoResponse>(
			`https://desktop.tidal.com/v1/tracks/${trackId}/playbackinfo?audioquality=${audioQuality}&playbackmode=STREAM&assetpresentation=FULL`,
		);
		if (result === undefined) this.unavailableTracks.add(trackId);
		return result;
	}

	public static async lyrics(trackId: redux.ItemId): Promise<redux.Lyrics | undefined> {
		return invokeIpc("tidal.lyrics", trackId);
	}
	public static async artist(artistId: redux.ItemId): Promise<redux.Artist | undefined> {
		return invokeIpc("tidal.artist", artistId);
	}

	public static async album(albumId: redux.ItemId): Promise<redux.Album | undefined> {
		return invokeIpc("tidal.album", albumId);
	}
	public static async albumItems(albumId: redux.ItemId): Promise<redux.MediaItem[] | undefined> {
		return invokeIpc("tidal.album_items", albumId);
	}

	public static async playlist(playlistUUID: redux.ItemId): Promise<redux.Playlist | undefined> {
		return invokeIpc("tidal.playlist", playlistUUID);
	}
	public static async playlistItems(playlistUUID: redux.ItemId) {
		return invokeIpc("tidal.playlist_items", playlistUUID) as Promise<{ items: redux.MediaItem[]; totalNumberOfItems: number; offset: number; limit: -1 } | undefined>;
	}

	public static async *isrc(isrc: string): AsyncIterable<TApiTrack> {
		const tracks: TApiTrack[] | undefined = await invokeIpc("tidal.isrc", isrc);
		if (tracks) yield* tracks;
	}
}
