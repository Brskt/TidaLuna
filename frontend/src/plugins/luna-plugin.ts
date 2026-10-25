/**
 * LunaPlugin - port of TidaLuna's LunaPlugin class for TidaLunar.
 *
 * This is a thin CEF-side proxy. It:
 * - Stores display state (name, url, enabled, installed, package)
 * - Routes lifecycle actions to Rust (DB) and Rust (runtime)
 * - Provides reactive signals for @luna/ui components
 * - Does NOT execute plugin code in CEF (execution is in Rust)
 */

import { Signal } from "./signal";
import { invokeIpc } from "../ipc";
import { store as obyStore } from "oby";

export type LunaAuthor = {
    name: string;
    url: string;
    avatarUrl?: string;
};

export type LunaPackageDependency = {
    name: string;
    storeUrl: string;
    devStoreUrl?: string;
};

export type LunaPackageMeta = {
    type?: "plugin" | "library";
    dependencies?: LunaPackageDependency[];
};

export type PluginPackage = {
    name: string;
    hash?: string;
    author?: LunaAuthor | string;
    homepage?: string;
    repository?: { type?: string; url?: string };
    description?: string;
    version?: string;
    dependencies?: string[];
    devDependencies?: string[];
    luna?: LunaPackageMeta;
};

export type LunaPluginStore = {
    url: string;
    package: PluginPackage;
    enabled: boolean;
    installed: boolean;
    liveReload: boolean;
    hideSettings: boolean;
};

type ModuleExports = {
    Settings?: any;
};

// Set by loader.ts after import. Allows lifecycle methods to sync
// both registries without a circular import on @luna/core.
let _upstreamPlugins: Record<string, any> | null = null;

/** Called by loader.ts to wire the upstream LunaPlugin.plugins reference. */
export function setUpstreamPlugins(plugins: Record<string, any>) {
    _upstreamPlugins = plugins;
}

/** Sync a plugin into the upstream registry and notify @luna/ui. */
function syncUpstream(url: string, plugin: LunaPlugin | null) {
    if (!_upstreamPlugins) return;
    if (plugin) {
        _upstreamPlugins[url] = plugin;
    } else {
        delete _upstreamPlugins[url];
    }
    window.dispatchEvent(new Event("luna:plugins-updated"));
}

export class LunaPlugin {
    // Static registry
    static plugins: Record<string, LunaPlugin> = {};
    static corePlugins = new Set<string>(["@luna/lib", "@luna/lib.native", "@luna/ui", "@luna/dev", "@luna/linux"]);

    // Instance properties
    name: string;
    url: string;
    store: LunaPluginStore;
    exports: ModuleExports | undefined;
    dependants = new Set<LunaPlugin>();

    // Reactive state (Signal-backed)
    loading = new Signal<boolean>(false);
    fetching = new Signal<boolean>(false);
    loadError = new Signal<string | undefined>(undefined);

    // Private
    private reloadTimeout: ReturnType<typeof setTimeout> | null = null;
    private enabledListeners = new Set<(enabled: boolean) => void>();
    private liveReloadListeners = new Set<(liveReload: boolean) => void>();

    constructor(store: LunaPluginStore) {
        this.store = obyStore(store);
        this.name = this.store.package.name;
        this.url = this.store.url;
    }

    get enabled(): boolean {
        return this.store.enabled;
    }

    set enabled(value: boolean) {
        this.store.enabled = value;
        for (const fn of this.enabledListeners) {
            try {
                fn(value);
            } catch (e) {
                console.error(e);
            }
        }
    }

    get installed(): boolean {
        return this.store.installed;
    }

    get liveReload(): boolean {
        return this.store.liveReload;
    }

    set liveReload(value: boolean) {
        this.store.liveReload = value;
        for (const fn of this.liveReloadListeners) {
            try {
                fn(value);
            } catch (e) {
                console.error(e);
            }
        }
        if (value && this.enabled) {
            this.startReloadLoop();
        } else {
            this.stopReloadLoop();
        }
    }

    get isDev(): boolean {
        return this.url.startsWith("http://127.0.0.1");
    }

    get isLibrary(): boolean {
        return this.store.package?.luna?.type === "library";
    }

    get dependencyRequirements(): LunaPackageDependency[] {
        return this.store.package?.luna?.dependencies ?? [];
    }

    dependsOn(pluginName: string): boolean {
        return this.dependencyRequirements.some(d => d.name === pluginName);
    }

    static getInstalled(): LunaPlugin[] {
        return Object.values(LunaPlugin.plugins).filter(p => p.store.installed);
    }

    static findInstalledDependantsOf(pluginName: string): LunaPlugin[] {
        return LunaPlugin.getInstalled().filter(p => p.dependsOn(pluginName));
    }

    onSetEnabled(callback: (enabled: boolean) => void): () => void {
        this.enabledListeners.add(callback);
        return () => {
            this.enabledListeners.delete(callback);
        };
    }

    onSetLiveReload(callback: (liveReload: boolean) => void): () => void {
        this.liveReloadListeners.add(callback);
        return () => {
            this.liveReloadListeners.delete(callback);
        };
    }

    // All actions follow: update local state -> persist to Rust DB -> route to Rust -> notify UI

    async load(): Promise<void> {
        // No-op: plugins are loaded into Rust at boot via jsrt.load_plugins.
        // This exists for API compatibility (upstream fromStorage calls it).
    }

    async install(): Promise<void> {
        this.loading._ = true;
        try {
            // 1. Store (installed=1, enabled=0)
            const info = await invokeIpc("plugin.install", this.url);
            try {
                const pkg = typeof info.manifest === "string" ? JSON.parse(info.manifest) : info.manifest;
                if (pkg?.name) this.store.package = pkg;
            } catch (e) { console.warn("[luna:plugin] Package info parse failed:", e); }
            if (info.hash) this.store.package.hash = info.hash;
            this.store.installed = true;
            this.name = this.store.package.name;
            LunaPlugin.plugins[this.url] = this;

            // 2. Enable atomically (Rust: check deps + DB enable + inject)
            try {
                await invokeIpc("plugin.enable", this.url);
                this.store.enabled = true;
                this.enabled = true;
            } catch (enableErr) {
                console.error(`[luna:plugin] Enable failed, attempting cleanup:`, enableErr);
                // Best-effort cleanup - guard may block if dependants exist
                let cleaned = false;
                try {
                    await invokeIpc("plugin.uninstall", this.url);
                    cleaned = true;
                } catch {
                    // Normal uninstall blocked - try dedicated cleanup for never-dispatched plugins
                    try {
                        await invokeIpc("plugin.cleanup_failed_install", this.url);
                        cleaned = true;
                    } catch {}
                }

                if (cleaned) {
                    this.store.installed = false;
                    delete LunaPlugin.plugins[this.url];
                    syncUpstream(this.url, null);
                } else {
                    // Zombie remains visible as installed=true, enabled=false
                    this.store.enabled = false;
                    this.enabled = false;
                    syncUpstream(this.url, this);
                }
                throw enableErr;
            }

            this.extractSettings();
            syncUpstream(this.url, this);
        } finally {
            this.loading._ = false;
        }
    }

    async enable(visited = new Set<string>()): Promise<void> {
        // Cycle = explicit error
        if (visited.has(this.url)) {
            console.error(`[luna:plugin] Circular dependency detected for '${this.name}'`);
            return;
        }
        visited.add(this.url);

        this.loading._ = true;
        this.loadError._ = undefined; // Clear any previous error (e.g., timeout from last attempt)
        try {
            // Auto-activate disabled dependencies (recursive)
            for (const dep of this.dependencyRequirements) {
                const depPlugin = LunaPlugin.getByName(dep.name);
                if (!depPlugin || !depPlugin.store.installed) {
                    console.error(`[luna:plugin] Missing dependency '${dep.name}' for '${this.name}'`);
                    return;
                }
                depPlugin.addDependant(this);
                if (!depPlugin.enabled) {
                    await depPlugin.enable(visited);
                    if (!depPlugin.enabled) {
                        console.error(`[luna:plugin] Dependency '${dep.name}' failed to enable`);
                        return;
                    }
                }
            }

            // Atomic: check deps + persist + inject
            await invokeIpc("plugin.enable", this.url);
            this.enabled = true;

            this.extractSettings();
            syncUpstream(this.url, this);
        } catch (e) {
            this.enabled = false;
            // On the card, not only in the console: the switch falling back to off says nothing.
            this.loadError._ = e instanceof Error ? e.message : String(e);
            console.error(`[luna:plugin] Failed to enable '${this.name}':`, e);
        } finally {
            this.loading._ = false;
        }
    }

    async disable(): Promise<void> {
        this.loading._ = true;
        try {
            this.stopReloadLoop();
            // Atomic: guard dependants + unload + persist (Rust handles everything)
            await invokeIpc("plugin.disable", this.url);
            // Only after Rust confirms:
            this.enabled = false;
            this.exports = undefined;
            if ((window as any).__pluginExports) {
                delete (window as any).__pluginExports[this.url];
            }
            syncUpstream(this.url, this);
        } finally {
            this.loading._ = false;
        }
        // NO catch - errors propagate to caller (reload, uninstall, UI)
    }

    async reload(): Promise<void> {
        this.loading._ = true;
        try {
            await this.disable(); // throws if Rust refuses (dependants)
            await this.enable();
        } catch (e) {
            console.error(`[luna:plugin] Failed to reload '${this.name}':`, e);
        } finally {
            this.loading._ = false;
        }
    }

    async uninstall(): Promise<void> {
        await this.disable();
        this.loading._ = true;
        try {
            // Rust guard may reject - only mutate local state after success
            await invokeIpc("plugin.uninstall", this.url);
            this.store.installed = false;
            delete LunaPlugin.plugins[this.url];
            syncUpstream(this.url, null);
            for (const dep of this.dependants) {
                await dep.uninstall();
            }
        } finally {
            this.loading._ = false;
        }
    }

    addDependant(plugin: LunaPlugin): void {
        this.dependants.add(plugin);
    }

    // Read the Settings component the running plugin registered on window.__pluginExports. The
    // wrapper registers it once the plugin's top level has finished, which is also what its ready
    // ack waits for: read after an enable, and again when Rust confirms the ready (loader.ts).
    // A plugin awaiting trust dialogs is ready long after its enable returns. Never throws: any
    // plugin can rig `window.__pluginExports`, and a throw here stopped the startup loop that lists
    // every plugin, the rigging one included.
    extractSettings(): boolean {
        let Settings: unknown;
        try {
            Settings = (window as any).__pluginExports?.[this.url]?.Settings;
        } catch (e) {
            console.error(`[luna:plugin] Settings unreadable for ${this.name}:`, e);
            return false;
        }
        if (!Settings) return false;
        if (this.exports?.Settings !== Settings) {
            this.exports = { Settings };
            console.log(`[luna:plugin] Settings found for ${this.name}`);
        }
        return true;
    }

    private startReloadLoop(): void {
        if (this.reloadTimeout) return;

        const poll = async () => {
            if (!this.liveReload || !this.enabled) return;
            this.fetching._ = true;
            try {
                // Read-only hash check - re-fetches code, computes hash, does NOT touch DB
                const result = await invokeIpc("plugin.check_hash", this.url);
                const newHash = result?.hash;
                if (newHash && newHash !== this.store.package.hash) {
                    this.store.package.hash = newHash;
                    await this.reload();
                }
            } catch (e) {
                console.error(`[luna:plugin] Live reload error for ${this.name}:`, e);
            } finally {
                this.fetching._ = false;
                if (this.liveReload && this.enabled) {
                    this.reloadTimeout = setTimeout(poll, 1000);
                }
            }
        };

        this.reloadTimeout = setTimeout(poll, 1000);
    }

    private stopReloadLoop(): void {
        if (this.reloadTimeout) {
            clearTimeout(this.reloadTimeout);
            this.reloadTimeout = null;
        }
    }

    static fromPluginInfo(info: any): LunaPlugin {
        let pkg: PluginPackage;
        try {
            pkg = typeof info.manifest === "string" ? JSON.parse(info.manifest) : info.manifest;
        } catch (e) {
            console.warn("[luna:plugin] Plugin info parse error:", e);
            pkg = { name: info.name };
        }
        if (!pkg.name) pkg.name = info.name;
        if (info.hash) pkg.hash = info.hash;

        const store: LunaPluginStore = {
            url: info.url,
            package: pkg,
            enabled: !!info.enabled,
            installed: !!info.installed,
            liveReload: false,
            hideSettings: true,
        };

        const plugin = new LunaPlugin(store);
        // Why Rust will not load this plugin, carried by its row: the startup pass that disables it
        // runs before this page listens; the row is the one channel that reaches the card.
        if (info.loadError) plugin.loadError._ = info.loadError;
        LunaPlugin.plugins[info.url] = plugin;
        return plugin;
    }

    static async install(url: string): Promise<LunaPlugin> {
        const cleanUrl = url.replace(/(\.(mjs|json|mjs\.map))$/, "");

        const existing = LunaPlugin.plugins[cleanUrl];
        if (existing) {
            await existing.disable();
        }

        const info = await invokeIpc("plugin.install", cleanUrl);
        const plugin = LunaPlugin.fromPluginInfo(info);

        // Enable atomically (Rust: check deps + persist + inject)
        try {
            await invokeIpc("plugin.enable", cleanUrl);
            plugin.store.enabled = true;
            plugin.enabled = true;
        } catch (enableErr) {
            console.error(`[luna:plugin] Enable failed, attempting cleanup:`, enableErr);
            let cleaned = false;
            try {
                await invokeIpc("plugin.uninstall", cleanUrl);
                cleaned = true;
            } catch {
                try {
                    await invokeIpc("plugin.cleanup_failed_install", cleanUrl);
                    cleaned = true;
                } catch {}
            }
            if (cleaned) {
                plugin.store.installed = false;
                delete LunaPlugin.plugins[cleanUrl];
                syncUpstream(cleanUrl, null);
            } else {
                plugin.store.enabled = false;
                plugin.enabled = false;
                syncUpstream(cleanUrl, plugin);
            }
            throw enableErr;
        }

        plugin.extractSettings();
        syncUpstream(cleanUrl, plugin);
        return plugin;
    }

    static async loadStoredPlugins(): Promise<void> {
        const plugins = await invokeIpc("plugin.list");
        if (!Array.isArray(plugins)) return;
        for (const info of plugins) {
            LunaPlugin.fromPluginInfo(info);
        }
    }

    static getByName(name: string): LunaPlugin | undefined {
        return Object.values(LunaPlugin.plugins).find(
            (p) => p.name === name && p.installed,
        );
    }

    static getByUrl(url: string): LunaPlugin | undefined {
        return LunaPlugin.plugins[url];
    }

    /**
     * Resolve a plugin from storage/URL - used by Plugin Store UI.
     * Fetches package metadata from {url}.json (Rust-side) for proper
     * name/author/description. Does NOT install or enable the plugin.
     */
    static async fromStorage(opts: { url: string; package?: PluginPackage }): Promise<LunaPlugin> {
        const url = opts.url.replace(/(\.(mjs|json|mjs\.map))$/, "");

        const existing = LunaPlugin.plugins[url];
        if (existing) return existing;

        let pkg = opts.package;
        if (!pkg?.name) {
            try {
                pkg = await invokeIpc("plugin.fetch_package", url);
                if (typeof pkg === "string") pkg = JSON.parse(pkg);
            } catch (e) {
                console.warn("[luna:plugin] Plugin update check error:", e);
                const lastSegment = url.split("/").filter(Boolean).pop() || "unknown";
                pkg = { name: lastSegment };
            }
        }

        const store: LunaPluginStore = {
            url,
            package: pkg!,
            enabled: false,
            installed: false,
            liveReload: false,
            hideSettings: true,
        };

        const plugin = new LunaPlugin(store);
        LunaPlugin.plugins[url] = plugin;
        return plugin;
    }

    /** Alias for store.package - used by Plugin Store UI */
    get package(): PluginPackage {
        return this.store.package;
    }
}
