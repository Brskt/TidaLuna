// TidaLunar: Extract stream format metadata from PlaybackInfo.
// For BTS/FLAC, bytes are NOT available from the API — they come from the Rust
// player bridge (mediaformat event) and are applied per-track in updateFormat().
// For DASH, bitrate/bytes are in the manifest.
import type { PlaybackInfo } from "../../helpers";
import { invokeIpc } from "../../../../../src/ipc";

type StreamFormat = { bitsPerSample?: number; sampleRate?: number; duration?: number; codec?: string };

export const parseStreamFormat = async (playbackInfo: PlaybackInfo): Promise<{ format: StreamFormat; bytes?: number }> => {
	const format: StreamFormat = {
		bitsPerSample: playbackInfo.bitDepth || undefined,
		sampleRate: playbackInfo.sampleRate || undefined,
	};

	if (playbackInfo.manifestMimeType === "application/vnd.tidal.bts") {
		format.codec = playbackInfo.manifest.codecs || (playbackInfo.mimeType?.includes("flac") ? "flac" : undefined);
		return { format, bytes: undefined };
	}

	if (playbackInfo.manifestMimeType === "application/dash+xml") {
		format.codec = playbackInfo.manifest.codec?.split(".")?.[0] || "aac";
		format.sampleRate = playbackInfo.manifest.sampleRate || format.sampleRate;
		return { format, bytes: undefined };
	}

	return { format, bytes: undefined };
};

export const getStreamBytes = async (playbackInfo: PlaybackInfo): Promise<number | undefined> => {
	if (playbackInfo.manifestMimeType !== "application/vnd.tidal.bts") return undefined;
	const url = playbackInfo.manifest.urls?.[0];
	if (!url) return undefined;
	try {
		const result = await invokeIpc("proxy.head", url);
		if (result?.status >= 200 && result?.status < 300 && result?.contentLength > 0) {
			return result.contentLength;
		}
	} catch {}
	return undefined;
};
