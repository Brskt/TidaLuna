/**
 * LunaPlugin — port of TidaLuna's LunaPlugin class for TidaLunar.
 *
 * Differences from upstream:
 * - No oby — uses Signal for reactive state
 * - No IndexedDB — persistence via SQLite/IPC (Rust backend)
 * - Fetch happens Rust-side via plugin.install IPC
 * - No @inrixia/helpers — Signal and Semaphore are local
 */

import { Signal } from "./signal";
import { Semaphore } from "./semaphore";
import { invokeIpc } from "../ipc";
import { modules, Tracer } from "./core";
import { store as obyStore } from "oby";

// --- Types matching TidaLuna's API ---

export type LunaAuthor = {
    name: string;
    url: string;
    avatarUrl?: string;
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
    unloads?: Set<() => void>;
    onUnload?: () => void;
    Settings?: any;
    errSignal?: Signal<string | undefined>;
};

// --- LunaPlugin class ---

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
    private blobUrl = "";
    private loadSemaphore = new Semaphore(1);
    private reloadTimeout: ReturnType<typeof setTimeout> | null = null;
    private enabledListeners = new Set<(enabled: boolean) => void>();
    private liveReloadListeners = new Set<(liveReload: boolean) => void>();

    constructor(store: LunaPluginStore) {
        this.store = obyStore(store);
        this.name = this.store.package.name;
        this.url = this.store.url;
    }

    // --- Getters / Setters ---

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

    // --- Event listeners ---

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

    // --- Lifecycle ---

    async load(): Promise<void> {
        if (this.enabled && this.installed) {
            await this.loadExports();
        }
    }

    async install(): Promise<void> {
        const info = await invokeIpc("plugin.install", this.url);
        // Update store with data from Rust
        try {
            const pkg = typeof info.manifest === "string" ? JSON.parse(info.manifest) : info.manifest;
            if (pkg?.name) this.store.package = pkg;
        } catch { /* keep existing package info */ }
        this.store.installed = true;
        this.store.enabled = true;
        this.enabled = true;
        this.name = this.store.package.name;
        await this.loadExports();
    }

    async enable(): Promise<void> {
        this.enabled = true;
        await invokeIpc("plugin.enable", this.url);
        await this.loadExports();
    }

    async disable(): Promise<void> {
        await this.unload();
        this.enabled = false;
        await invokeIpc("plugin.disable", this.url);
    }

    async reload(): Promise<void> {
        await this.unload();
        await this.loadExports(true);
    }

    async uninstall(): Promise<void> {
        await this.disable();
        this.store.installed = false;
        delete LunaPlugin.plugins[this.url];
        await invokeIpc("plugin.uninstall", this.url);
        // Cascade to dependants
        for (const dep of this.dependants) {
            await dep.uninstall();
        }
    }

    addDependant(plugin: LunaPlugin): void {
        this.dependants.add(plugin);
    }

    // --- Private: module loading ---

    private async loadExports(reload = false): Promise<void> {
        await this.loadSemaphore.acquire();
        try {
            this.loading._ = true;
            this.loadError._ = undefined;

            const code = await invokeIpc("plugin.get_code", this.url);
            if (!code) {
                this.loadError._ = "No code available";
                return;
            }

            const blob = new Blob([code], { type: "application/javascript" });
            const blobUrl = URL.createObjectURL(blob);
            this.blobUrl = blobUrl;

            try {
                const mod = await import(/* @vite-ignore */ blobUrl);
                this.exports = {};

                if (mod.unloads instanceof Set) {
                    this.exports.unloads = mod.unloads;
                }
                if (typeof mod.onUnload === "function") {
                    this.exports.onUnload = mod.onUnload;
                }
                if (mod.Settings) {
                    this.exports.Settings = mod.Settings;
                }
                if (mod.errSignal) {
                    this.exports.errSignal = mod.errSignal;
                }

                // Register in module registry so window.require(name) works
                modules[this.name] = mod;

                const { trace } = Tracer(this.name);
                trace(reload ? "Reloaded" : "Loaded");
            } catch (e: any) {
                this.loadError._ = e?.message || "Failed to load";
                console.error(`[luna:plugin] Failed to load ${this.name}:`, e);
                if (this.blobUrl) {
                    URL.revokeObjectURL(this.blobUrl);
                    this.blobUrl = "";
                }
            }
        } finally {
            this.loading._ = false;
            this.loadSemaphore.release();
        }
    }

    // --- Private: cleanup ---

    private async unload(): Promise<void> {
        this.stopReloadLoop();

        if (this.exports) {
            // Run cleanup functions in reverse order
            if (this.exports.unloads) {
                const cleanups = Array.from(this.exports.unloads);
                for (let i = cleanups.length - 1; i >= 0; i--) {
                    try {
                        cleanups[i]();
                    } catch (e) {
                        console.error(`[luna:plugin] Cleanup error in ${this.name}:`, e);
                    }
                }
            }
            if (this.exports.onUnload) {
                try {
                    this.exports.onUnload();
                } catch (e) {
                    console.error(`[luna:plugin] onUnload error in ${this.name}:`, e);
                }
            }
            this.exports = undefined;
        }

        delete modules[this.name];

        if (this.blobUrl) {
            URL.revokeObjectURL(this.blobUrl);
            this.blobUrl = "";
        }

        // Cascade: unload dependant plugins
        for (const dep of this.dependants) {
            await dep.unload();
        }
    }

    // --- Private: live reload polling ---

    private startReloadLoop(): void {
        if (this.reloadTimeout) return;

        const poll = async () => {
            if (!this.liveReload || !this.enabled) return;
            this.fetching._ = true;
            try {
                // Re-install to refresh cached code (Rust fetches + updates SQLite)
                const info = await invokeIpc("plugin.install", this.url);
                const newHash = info?.hash;
                if (newHash && newHash !== this.store.package.hash) {
                    this.store.package.hash = newHash;
                    await this.unload();
                    await this.loadExports(true);
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

    // --- Static factory methods ---

    static fromPluginInfo(info: any): LunaPlugin {
        let pkg: PluginPackage;
        try {
            pkg = typeof info.manifest === "string" ? JSON.parse(info.manifest) : info.manifest;
        } catch {
            pkg = { name: info.name };
        }
        if (!pkg.name) pkg.name = info.name;

        const store: LunaPluginStore = {
            url: info.url,
            package: pkg,
            enabled: !!info.enabled,
            installed: !!info.installed,
            liveReload: false,
            hideSettings: false,
        };

        const plugin = new LunaPlugin(store);
        LunaPlugin.plugins[info.url] = plugin;
        return plugin;
    }

    static async install(url: string): Promise<LunaPlugin> {
        // Sanitize URL like upstream
        const cleanUrl = url.replace(/(\.(mjs|json|mjs\.map))$/, "");

        // If already loaded at this URL, disable first (unloads runtime + marks disabled)
        // Rust install will set enabled=1 again via INSERT OR REPLACE
        const existing = LunaPlugin.plugins[cleanUrl];
        if (existing) {
            await existing.disable();
        }

        const info = await invokeIpc("plugin.install", cleanUrl);
        const plugin = LunaPlugin.fromPluginInfo(info);
        if (plugin.enabled) {
            await plugin.loadExports();
        }
        return plugin;
    }

    static async loadStoredPlugins(): Promise<void> {
        const plugins = await invokeIpc("plugin.list");
        if (!Array.isArray(plugins)) return;
        for (const info of plugins) {
            const plugin = LunaPlugin.fromPluginInfo(info);
            await plugin.load();
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
     * Resolve a plugin from storage/URL — used by Plugin Store UI.
     * Fetches package metadata from {url}.json (Rust-side) for proper
     * name/author/description. Does NOT install or enable the plugin.
     */
    static async fromStorage(opts: { url: string; package?: PluginPackage }): Promise<LunaPlugin> {
        // Sanitize URL: strip extensions like upstream
        const url = opts.url.replace(/(\.(mjs|json|mjs\.map))$/, "");

        const existing = LunaPlugin.plugins[url];
        if (existing) return existing;

        let pkg = opts.package;
        if (!pkg?.name) {
            try {
                pkg = await invokeIpc("plugin.fetch_package", url);
                if (typeof pkg === "string") pkg = JSON.parse(pkg);
            } catch {
                // Fallback: derive name from URL
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
            hideSettings: false,
        };

        const plugin = new LunaPlugin(store);
        LunaPlugin.plugins[url] = plugin;
        return plugin;
    }

    /** Alias for store.package — used by Plugin Store UI */
    get package(): PluginPackage {
        return this.store.package;
    }
}
