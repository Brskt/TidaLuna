/**
 * Plugin installation orchestration with library dependency resolution.
 * Adapted from upstream TidaLuna PR #146 for our Rust-backed architecture.
 *
 * JS orchestrates UX (prompts, sequencing), Rust guards invariants.
 */
import { LunaPlugin, Tracer } from "@luna/core";
import type { LunaPackageDependency } from "@luna/core";
import { confirm } from "../../helpers/confirm";
import { addToStores, storeUrls } from "./storeState";

type StorePackage = { plugins: string[] };

const trace = Tracer("[@luna/ui][PluginInstall]", null).trace;

// --- URL normalization ---

const normalizeStoreUrl = (storeUrl: string) => storeUrl.replace(/\/store\.json$/i, "");
const normalizePluginUrl = (pluginUrl: string) => pluginUrl.replace(/(\.mjs|\.json|\.mjs\.map)$/i, "");
const isStoreAdded = (storeUrl: string) => storeUrls.includes(normalizeStoreUrl(storeUrl));
const isBuiltInDevStore = (storeUrl: string) => /^https?:\/\/(127\.0\.0\.1|localhost):3000$/i.test(normalizeStoreUrl(storeUrl));
const isFromDevStore = (plugin: LunaPlugin) => /^https?:\/\/(127\.0\.0\.1|localhost):3000\//i.test(plugin.url);
const getDependencyStoreUrl = (dependency: LunaPackageDependency, dependantPlugin: LunaPlugin) => {
	if (isFromDevStore(dependantPlugin) && dependency.devStoreUrl) return dependency.devStoreUrl;
	return dependency.storeUrl;
};

// --- Name → URL resolution ---

const toPluginIdCandidates = (value: string): string[] => {
	const trimmed = value.trim();
	const withoutScopeAt = trimmed.replace(/^@/, "");
	const safeName = trimmed.replace(/@/g, "").replace(/\//g, ".");
	const dottedScope = withoutScopeAt.replace(/\//g, ".");
	const atWithDots = trimmed.replace(/\//g, ".");
	const spaced = trimmed.replace(/\s+/g, ".");
	return [...new Set([trimmed, withoutScopeAt, safeName, dottedScope, atWithDots, spaced].filter(Boolean))];
};

const toPluginUrlCandidates = (storeUrl: string, pluginId: string): string[] => {
	const normalizedStoreUrl = normalizeStoreUrl(storeUrl);
	const isLocalDevStore = isBuiltInDevStore(normalizedStoreUrl);
	if (/^https?:\/\//i.test(pluginId)) return [normalizePluginUrl(pluginId)];
	const candidates = toPluginIdCandidates(pluginId);
	if (isLocalDevStore) return candidates.map((c) => normalizePluginUrl(`${normalizedStoreUrl}/${c}`));
	return candidates.map((c) => normalizePluginUrl(`${normalizedStoreUrl}/${c.replace(/\s+/g, ".")}`));
};

const resolvedPluginUrlCache = new Map<string, string>();

const getStorePluginUrls = async (storeUrl: string): Promise<string[]> => {
	const normalizedStoreUrl = normalizeStoreUrl(storeUrl);
	const response = await fetch(`${normalizedStoreUrl}/store.json`);
	if (!response.ok) { trace.msg.err(`Failed to fetch plugin store '${normalizedStoreUrl}': ${response.statusText}`); return []; }
	const storePackage = (await response.json()) as StorePackage;
	return [...new Set((storePackage.plugins ?? []).flatMap((pluginId) => toPluginUrlCandidates(normalizedStoreUrl, pluginId)))];
};

const tryResolveFromUrls = async (pluginUrls: string[], pluginName: string, cacheKey: string): Promise<string | undefined> => {
	for (const pluginUrl of pluginUrls) {
		try {
			const pkg = await LunaPlugin.fetchPackage(normalizePluginUrl(pluginUrl));
			if (pkg.name !== pluginName) continue;
			resolvedPluginUrlCache.set(cacheKey, normalizePluginUrl(pluginUrl));
			return normalizePluginUrl(pluginUrl);
		} catch { /* continue */ }
	}
};

const resolvePluginUrlByName = async (storeUrl: string, pluginName: string): Promise<string | undefined> => {
	const normalizedStoreUrl = normalizeStoreUrl(storeUrl);
	const cacheKey = `${normalizedStoreUrl}::${pluginName}`;
	const cached = resolvedPluginUrlCache.get(cacheKey);
	if (cached) return cached;
	const pluginUrls = await getStorePluginUrls(normalizedStoreUrl);
	const resolvedFromStore = await tryResolveFromUrls(pluginUrls, pluginName, cacheKey);
	if (resolvedFromStore) return resolvedFromStore;
	const directCandidates = toPluginUrlCandidates(normalizedStoreUrl, pluginName);
	const resolvedFromDirect = await tryResolveFromUrls(directCandidates, pluginName, cacheKey);
	if (resolvedFromDirect) return resolvedFromDirect;
	trace.msg.err(`Failed to resolve dependency '${pluginName}' from store '${normalizedStoreUrl}'.`);
	return;
};

// --- User confirmation prompts ---

const ensureStoreForDependency = async ({ name, storeUrl }: LunaPackageDependency, dependantName: string) => {
	const normalizedStoreUrl = normalizeStoreUrl(storeUrl);
	if (isBuiltInDevStore(normalizedStoreUrl)) return true;
	if (isStoreAdded(normalizedStoreUrl)) return true;
	try {
		await confirm({ title: "Add dependency store?", description: `Plugin '${dependantName}' requires library '${name}' from '${normalizedStoreUrl}'. Add this store now?`, confirmationText: "Add store" });
	} catch { trace.msg.err(`Install cancelled: '${dependantName}' needs '${name}', but its plugin store was not added.`); return false; }
	addToStores(normalizedStoreUrl);
	return true;
};

const confirmInstallDependency = async ({ name }: LunaPackageDependency, dependantName: string) => {
	try {
		await confirm({ title: "Install required library?", description: `Plugin '${dependantName}' depends on library plugin '${name}'. Install it now?`, confirmationText: "Install library" });
	} catch { trace.msg.err(`Install cancelled: '${dependantName}' requires library '${name}'.`); return false; }
	return true;
};

// --- Core install logic ---

const installDependency = async (dependency: LunaPackageDependency, dependantPlugin: LunaPlugin, visited: Set<string>) => {
	const dependencyStoreUrl = getDependencyStoreUrl(dependency, dependantPlugin);
	const dependencyWithStore = { ...dependency, storeUrl: dependencyStoreUrl };
	const existingDependency = LunaPlugin.getByName(dependency.name);
	if (existingDependency?.installed) return existingDependency;
	if (existingDependency !== undefined) {
		if (!(await confirmInstallDependency(dependencyWithStore, dependantPlugin.name))) return;
		const didInstall = await installPluginWithLibraries(existingDependency, visited);
		if (didInstall && existingDependency.installed) return existingDependency;
	}
	if (!(await ensureStoreForDependency(dependencyWithStore, dependantPlugin.name))) return;
	if (!(await confirmInstallDependency(dependencyWithStore, dependantPlugin.name))) return;
	const dependencyUrl = await resolvePluginUrlByName(dependencyWithStore.storeUrl, dependencyWithStore.name);
	if (dependencyUrl === undefined) { trace.msg.err(`Could not find dependency '${dependency.name}'`); return; }
	const dependencyPlugin = await LunaPlugin.fromStorage({ url: dependencyUrl });
	const didInstall = await installPluginWithLibraries(dependencyPlugin, visited);
	if (!didInstall) return;
	return dependencyPlugin;
};

// --- Public API ---

export const installPluginWithLibraries = async (plugin: LunaPlugin, visited = new Set<string>()) => {
	if (visited.has(plugin.name)) return true;
	visited.add(plugin.name);
	try {
		for (const dependency of plugin.dependencyRequirements) {
			const dependencyPlugin = await installDependency(dependency, plugin, visited);
			if (dependencyPlugin === undefined || !dependencyPlugin.installed) {
				trace.msg.err(`Skipping install of '${plugin.name}' because dependency '${dependency.name}' is unavailable.`);
				return false;
			}
		}
		if (!plugin.installed) {
			await plugin.install();
			if (plugin.isLibrary) trace.msg.log(`Installed library plugin ${plugin.name}.`);
		}
		return plugin.installed;
	} catch (err) {
		trace.msg.err.withContext(`Failed to install plugin '${plugin.name}'`)(err);
		return false;
	}
};

export const uninstallPluginWithDependenciesCheck = async (plugin: LunaPlugin) => {
	try {
		await plugin.uninstall();
		return !plugin.installed;
	} catch (err) {
		trace.msg.err.withContext(`Failed to uninstall plugin '${plugin.name}'`)(err);
		return false;
	}
};
