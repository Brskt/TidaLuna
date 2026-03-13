import type { DashManifest } from "./getPlaybackInfo.dasha.native";

import type { PlaybackInfoResponse } from "../classes/TidalApi";

import type { AudioQuality, ItemId } from "../redux";

import { invokeIpc } from "../../ipc";

interface PlaybackInfoBase extends Omit<PlaybackInfoResponse, "manifest"> {
	mimeType: string;
}

export type TidalManifest = {
	mimeType: string;
	codecs: string;
	encryptionType: string;
	keyId: string;
	urls: string[];
};
interface TidalPlaybackInfo extends PlaybackInfoBase {
	manifestMimeType: "application/vnd.tidal.bts";
	manifest: TidalManifest;
}
interface DashPlaybackInfo extends PlaybackInfoBase {
	manifestMimeType: "application/dash+xml";
	manifest: DashManifest;
}
export type PlaybackInfo = DashPlaybackInfo | TidalPlaybackInfo;

export const getPlaybackInfo = async (mediaItemId: ItemId, audioQuality: AudioQuality): Promise<PlaybackInfo | undefined> => {
	// Rust decodes the base64 manifest and returns the processed PlaybackInfo
	const result = await invokeIpc("tidal.playback_info", mediaItemId, audioQuality);
	if (result === null || result === undefined) return undefined;

	// Map the Rust response (with decodedManifest) to the TS PlaybackInfo shape
	const { decodedManifest, ...rest } = result;
	if (decodedManifest?.type === "bts") {
		const manifest: TidalManifest = decodedManifest;
		return { ...rest, manifestMimeType: "application/vnd.tidal.bts" as const, manifest, mimeType: rest.mimeType ?? manifest.mimeType };
	} else {
		const manifest: DashManifest = decodedManifest ?? { tracks: { audios: [] } };
		return { ...rest, manifestMimeType: "application/dash+xml" as const, manifest, mimeType: "audio/mp4" };
	}
};
