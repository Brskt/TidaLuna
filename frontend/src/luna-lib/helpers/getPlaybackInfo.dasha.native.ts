// Stub: DASH parsing is handled by the Rust backend in TidaLunar
export type DashManifest = {
	tracks: { audios: { bitrate: { bps?: number }; size?: { b?: number } }[] };
};
export type DashaParseArgs = [string, ...any[]];

export const parseDasha = async (..._args: DashaParseArgs): Promise<DashManifest> => {
	return { tracks: { audios: [] } };
};
