import type { PlaybackInfo } from "../../helpers";
import type { MetaTags } from "./MediaItem.tags";
import { invokeIpc } from "../../../../../src/ipc";

// Undefined tells a caller there is nothing to poll, which both download plugins handle
// by stopping their progress loop. Rust reports no progress, so this stays as it is.
export const downloadProgress = async (_trackId: any) => undefined;

// The renderer cannot write to disk, so Rust takes the manifest, destination and tags and
// writes the file. `encryptionType` is forwarded so Rust decrypts on the manifest's answer
// rather than guessing from the key; `codecs` is not, it mislabels real FLAC.
export const download = async (playbackInfo: PlaybackInfo, path: string | string[], tags?: MetaTags): Promise<void> => {
	if (playbackInfo.manifestMimeType !== "application/vnd.tidal.bts")
		throw new Error(`Downloads of ${playbackInfo.manifestMimeType} are not supported`);
	await invokeIpc(
		"plugin.download",
		JSON.stringify({
			manifestMimeType: playbackInfo.manifestMimeType,
			urls: playbackInfo.manifest.urls,
			keyId: playbackInfo.manifest.keyId,
			encryptionType: playbackInfo.manifest.encryptionType,
			path: Array.isArray(path) ? path.join("/") : path,
			tags,
		}),
	);
};
