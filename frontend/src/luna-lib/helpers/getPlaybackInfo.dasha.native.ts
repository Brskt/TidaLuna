export type DashManifest = {
	initUrl: string;
	segmentUrls: string[];
	codec: string;
	sampleRate?: number;
	bandwidth?: number;
};
export type DashaParseArgs = [string, ...any[]];

export const parseDasha = async (..._args: DashaParseArgs): Promise<DashManifest> => {
	return { initUrl: "", segmentUrls: [], codec: "" };
};
