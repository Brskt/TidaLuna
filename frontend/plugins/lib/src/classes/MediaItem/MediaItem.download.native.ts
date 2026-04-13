// Stub: download functionality is handled by the Rust backend in TidaLunar
import type { PlaybackInfo } from "../../helpers";
import type { MetaTags } from "./MediaItem.tags";

export const downloadProgress = async (_trackId: any) => undefined;
export const download = async (_playbackInfo: PlaybackInfo, _path: string | string[], _tags?: MetaTags): Promise<void> => {
	throw new Error("Download is not supported in TidaLunar browser context - handled by Rust backend");
};
