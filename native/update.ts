import { mkdir, readdir, readFile, rm, unlink, writeFile } from "fs/promises";
import JSZip from "jszip";
import path from "path";

import { app } from "electron";

import type { PackageJson } from "type-fest";

export const pkg = async (): Promise<PackageJson> => JSON.parse(await readFile(process.resourcesPath + "/app/package.json", "utf-8"));

export const relaunch = async () => {
	app.relaunch();
	app.exit(0);
};

export const reload = async () => {
	const win = globalThis.luna.tidalWindow;
	if (!win) return;
	win.loadURL("https://desktop.tidal.com/");
};

const appFolder = path.join(process.resourcesPath, "app");

export const update = async (version: string) => {
	const zipUrl = `https://github.com/Inrixia/TidaLuna/releases/download/${version}/luna.zip`;
	const res = await fetch(zipUrl);
	if (!res.ok) throw new Error(`Failed to download ${zipUrl}\n${res.statusText}`);

	// Ensure clean start

	console.log(`[UPDATER] == Downloaded: ${zipUrl}`);

	// Load zip purely from buffer (no internal fs usage by the library)
	const zip = await JSZip.loadAsync(Buffer.from(await res.arrayBuffer()));

	console.log("[UPDATER] == Loaded zip into memory");

	await clearAppFolder();
	await mkdir(appFolder, { recursive: true });

	console.log("[UPDATER] == Cleared app folder");

	// Manually write files to disk
	const entries = Object.keys(zip.files);
	for (const filename of entries) {
		const file = zip.files[filename];
		const destPath = path.join(appFolder, filename);

		// Security: Prevent Zip Slip (directory traversal attacks)
		if (!destPath.startsWith(appFolder)) {
			console.warn(`[UPDATER] == Skipping unsafe path: ${filename}`);
			continue;
		}

		if (file.dir) {
			await mkdir(destPath, { recursive: true });
		} else {
			// Ensure parent directory exists (zip entries aren't always sorted)
			await mkdir(path.dirname(destPath), { recursive: true });

			// Convert to Node Buffer and write
			const content = await file.async("nodebuffer");
			await writeFile(destPath, content);
		}
	}

	console.log("[UPDATER] == Extraction complete");

	await reload();
};

const clearAppFolder = async () => {
	// Check if folder exists before reading to avoid crashing on fresh installs
	try {
		const entries = await readdir(appFolder, { withFileTypes: true });
		for (const entry of entries) {
			const fullPath = path.join(appFolder, entry.name);
			if (entry.isDirectory()) await rm(fullPath, { recursive: true, force: true });
			else await unlink(fullPath);
		}
	} catch (error: any) {
		if (error.code !== "ENOENT") throw error;
	}
};
