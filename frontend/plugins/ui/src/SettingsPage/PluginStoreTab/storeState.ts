import { ReactiveStore } from "@luna/core";
import { ipcRenderer } from "@luna/lib";

const pluginStores = ReactiveStore.getStore("@luna/pluginStores");
export const storeUrls = await pluginStores.getReactive<string[]>("storeUrls", []);
const normalizeStoreUrl = (url: string) => (url.endsWith("/store.json") ? url.slice(0, -11) : url);
export const addToStores = (url: string) => {
	url = normalizeStoreUrl(url);
	if (storeUrls.includes(url)) return false;
	return storeUrls.push(url);
};

// Swaps old store urls for the new one <3 (call addToStores after to add)
export const replaceStore = async (legacyUrls: string[], newUrl: string) => {
	newUrl = normalizeStoreUrl(newUrl);
	// Ignore case.. tidaluna:// links lowercase urls <3
	const legacy = new Set(legacyUrls.map((url) => normalizeStoreUrl(url).toLowerCase()));
	// The installed plugins live in Rust, which asks the user before moving them. A failure must not
	// keep @luna/ui from loading: the store list below still moves.
	for (const legacyUrl of legacy) {
		await ipcRenderer.invoke("plugin.migrate_base_url", legacyUrl, newUrl).catch((e) => {
			// Once a plugin has run in this process, as after a reload, Rust refuses the question with a
			// 403: this launch asked it at its first page, and what was left open is asked at the next launch.
			if (e?.code === 403) console.log("[@luna/ui] store move not asked:", e.message);
			else console.error("[@luna/ui] store move failed:", e);
		});
	}
	for (let i = 0; i < storeUrls.length; i++) {
		const url = storeUrls[i];
		if (url === undefined || !legacy.has(normalizeStoreUrl(url).toLowerCase())) continue;
		if (storeUrls.includes(newUrl)) storeUrls.splice(i--, 1);
		else storeUrls[i] = newUrl;
	}
};
