import React from "react";

import { type PluginPackage } from "@luna/core";

import Grid from "@mui/material/Grid";
import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";

import { LunaTrashButton, SpinningButton } from "../../components";
import { LunaPluginHeader } from "../PluginsTab/LunaPluginHeader";
import { LunaStorePlugin } from "./LunaStorePlugin";

interface StorePackage extends PluginPackage {
	plugins: string[];
}

// Cap the store fan-out: React mounts one self-fetching LunaStore per URL at once, so
// without a cap every store fetches in one burst; 6 matches Chromium's per-host connection
// cap (the default stores all share the github.com origin).
const MAX_CONCURRENT_STORE_FETCHES = 6;

// Frees a pool slot if a store stalls: kept under the proxy's own 15s budget so one slow
// store never holds a slot long enough to stall the others queued behind the cap.
const STORE_FETCH_TIMEOUT_MS = 10000;

// Bounded async semaphore: hands a freed slot straight to the next waiter (FIFO) so
// concurrent callers can never exceed `max`.
const limitConcurrency = (max: number) => {
	let active = 0;
	const waiters: Array<() => void> = [];
	const acquire = () => {
		if (active < max) {
			active++;
			return Promise.resolve();
		}
		return new Promise<void>((resolve) => waiters.push(resolve));
	};
	const release = () => {
		const next = waiters.shift();
		if (next) next();
		else active--;
	};
	return async <T,>(task: () => Promise<T>): Promise<T> => {
		await acquire();
		try {
			return await task();
		} finally {
			release();
		}
	};
};

const runStoreFetch = limitConcurrency(MAX_CONCURRENT_STORE_FETCHES);

export const LunaStore = React.memo(({ url, onRemove, searchQuery }: { url: string; onRemove: () => void; searchQuery: string }) => {
	const [loading, setLoading] = React.useState(false);
	const [loadError, setLoadError] = React.useState<string | undefined>(undefined);
	const [pkg, setPackage] = React.useState<StorePackage | undefined>(undefined);

	const disabled = loading; // Disable controls while loading

	const fetchPackage = React.useCallback(async () => {
		setLoading(true);
		setLoadError(undefined);
		try {
			const data = await runStoreFetch(async () => {
				const controller = new AbortController();
				const timer = setTimeout(() => controller.abort(), STORE_FETCH_TIMEOUT_MS);
				try {
					const response = await fetch(`${url}/store.json`, { signal: controller.signal });
					if (!response.ok) throw new Error(`Failed to fetch package: ${response.statusText}`);
					return await response.json();
				} finally {
					clearTimeout(timer);
				}
			});
			setPackage(data);
		} catch (error: any) {
			console.error("Error fetching package:", error);
			setLoadError(error.message || "Unknown error occurred");
			setPackage(undefined); // Clear package on error
		} finally {
			setLoading(false);
		}
	}, [url]);

	React.useEffect(() => {
		fetchPackage();
	}, [fetchPackage]); // Depend on the memoized fetch function

	const isLocalDevStore = url === "http://127.0.0.1:3000";

	if (pkg === undefined && !loading && !loadError) return null; // Don't render anything until initial load attempt
	if (!isLocalDevStore && loading && !pkg) return <Typography>Loading store {url}...</Typography>; // Show loading indicator if still loading initially

	let name = pkg?.name ?? "Unknown Store";
	if (isLocalDevStore) name = `${name} [DEV]`;

	const author = pkg?.author;
	const desc = pkg?.description;

	const link = pkg?.homepage ?? pkg?.repository?.url ?? url;

	// Don't show error for local dev store
	if (loadError && isLocalDevStore) return null;

	const query = searchQuery.toLowerCase();
	const filteredPlugins = query ? pkg?.plugins.filter((plugin) => plugin.toLowerCase().includes(query)) : pkg?.plugins;
	if (query && (!filteredPlugins || filteredPlugins.length === 0)) return null;

	return (
		<Stack
			spacing={1}
			sx={{
				borderRadius: 3,
				backgroundColor: "rgba(0, 0, 0, 0.10)",
				boxShadow: loadError ? "0 0 10px rgba(255, 0, 0, 0.70)" : "none",
				padding: 2,
			}}
		>
			<LunaPluginHeader
				name={name}
				link={link}
				loadError={loadError}
				author={author}
				desc={desc}
				children={
					<>
						<SpinningButton title="Reload store" spin={loading} disabled={disabled} onClick={fetchPackage} />
						<LunaTrashButton disabled={isLocalDevStore} title="Remove store" onClick={onRemove} />
					</>
				}
			/>
			<Grid columns={2} spacing={2} container>
				{filteredPlugins?.map((plugin) => (
					<Grid size={1} children={<LunaStorePlugin url={`${url}/${isLocalDevStore ? plugin : plugin.replace(" ", ".")}`} key={plugin} />} />
				))}
			</Grid>
		</Stack>
	);
});
