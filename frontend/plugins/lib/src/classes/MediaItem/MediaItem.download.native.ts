import type { PlaybackInfo } from "../../helpers";
import type { MetaTags } from "./MediaItem.tags";
import { invokeIpc } from "../../../../../src/ipc";

// Undefined tells a caller there is nothing to poll, which both download plugins handle
// by stopping their progress loop. Rust reports no progress, so this stays as it is.
export const downloadProgress = async (_trackId: any) => undefined;

// The renderer cannot write to disk, so Rust takes the urls, destination and tags and writes the
// file. `encryptionType` is forwarded so Rust decrypts on the manifest's answer rather than guessing
// from the key; `codecs` is not, it mislabels real FLAC.
//
// Both manifests reduce to a url list Rust fetches in order and concatenates. BTS gives one; DASH
// gives its init segment followed by its media segments, which concatenate into a fragmented MP4.
// That plays as written but carries no tags, since tagging an MP4 needs a remux this does not do.
export const download = async (playbackInfo: PlaybackInfo, path: string | string[], tags?: MetaTags): Promise<void> => {
	const dest = Array.isArray(path) ? path.join("/") : path;
	let payload: Record<string, unknown>;
	switch (playbackInfo.manifestMimeType) {
		case "application/vnd.tidal.bts":
			payload = {
				urls: playbackInfo.manifest.urls,
				keyId: playbackInfo.manifest.keyId,
				encryptionType: playbackInfo.manifest.encryptionType,
				tags,
			};
			break;
		case "application/dash+xml":
			payload = { urls: [playbackInfo.manifest.initUrl, ...playbackInfo.manifest.segmentUrls] };
			break;
		default:
			throw new Error(`Downloads of ${(playbackInfo as PlaybackInfo).manifestMimeType} are not supported`);
	}
	await invokeIpc(
		"plugin.download",
		JSON.stringify({ manifestMimeType: playbackInfo.manifestMimeType, path: dest, ...payload }),
	);
};
