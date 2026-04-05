export interface UpdateInfo {
	version: string;
	download_size: number;
}

export type UpdaterPhase = "available" | "downloading" | "ready" | "error";
