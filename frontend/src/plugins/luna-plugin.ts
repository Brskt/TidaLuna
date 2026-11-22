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
import { describeError } from "./listener-owner";
import { leafFirstGroups } from "./reload-order";
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

// What `plugin.update` found at the plugin's source (Rust `Pulled`). It stores and never restarts:
// the Reload button restarts what is stored unless `busy` or `gone`, the poll only on `updated`.
type Pulled =
    | { outcome: "updated"; hash: string; manifest: string }
    | { outcome: "upToDate"; hash: string }
    | { outcome: "refused"; hash: string; reason: string }
    | { outcome: "blocked" | "unavailable"; reason: string }
    | { outcome: "busy" | "gone" };

function pullNote(pulled: Pulled): string {
    switch (pulled.outcome) {
        case "updated": return "Updated to the latest version";
        case "upToDate": return "Already up to date";
        case "refused": return `The source's version is refused: ${pulled.reason}`;
        case "blocked": return pulled.reason;
        case "unavailable": return `Not updated, the installed version runs: ${pulled.reason}`;
        case "busy": return "An update of this plugin is already running";
        case "gone": return "This plugin is no longer installed";
    }
}

// The name a card shows for a url whose package is not known yet.
function nameFromUrl(url: string): string {
    return url.split("/").filter(Boolean).pop() || "unknown";
}

// Rust refused to stop the plugin, which runs as it was; an update it pulled waits for a launch.
function stopRefusedNote(pulled: Pulled, why: string): string {
    return pulled.outcome === "updated" ? `Updated, runs from the next launch: ${why}` : `Not restarted: ${why}`;
}

export type ReloadAllResult = {
    reloaded: string[];
    notReloaded: { name: string; reason: string }[];
};

const CHANGED_DURING_RELOAD = "Turned off or removed while reloading";

// The live-reload poll's pace, and its ceiling for a failing source or a blocked update.
const POLL_MS = 1000;
const POLL_MAX_MS = 30000;

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

    // Set by the first lifecycle action: from then on the state is what that action left (Rust's
    // answer, then the row it read back), and a row read before it must not overwrite that.
    acted = false;

    // Reactive state (Signal-backed)
    loading = new Signal<boolean>(false);
    fetching = new Signal<boolean>(false);
    loadError = new Signal<string | undefined>(undefined);
    // A listener of the running plugin threw: shown on its card, the plugin runs on.
    runtimeError = new Signal<string | undefined>(undefined);

    // Private
    private reloadTimeout: ReturnType<typeof setTimeout> | null = null;
    // Bumped by each start and stop: a poll of a loop since stopped does not schedule another.
    private pollLoop = 0;
    // Bumped by each enable, stop and write of `enabled`: a row read begun before any of them
    // is stale.
    private rowEpoch = 0;
    // The last source hash the poll had a verdict on; only a different one is pulled again.
    private seenHash: string | undefined;
    // Held by a Reload all from its pull to its restart: `loading` alone is cleared by each step.
    private held = false;
    private static reloadingAll: Promise<ReloadAllResult> | undefined;
    private enabledListeners = new Set<(enabled: boolean) => void>();
    private liveReloadListeners = new Set<(liveReload: boolean) => void>();
    // What a Reload found, or why the live reload did not take a version; a failed start is a
    // loadError. Each note is told, even one equal to the last: a Signal would drop it.
    private noteListeners = new Set<(note: string | undefined) => void>();

    constructor(store: LunaPluginStore) {
        this.store = obyStore(store);
        this.name = this.store.package.name;
        this.url = this.store.url;
    }

    get enabled(): boolean {
        return this.store.enabled;
    }

    set enabled(value: boolean) {
        this.rowEpoch++;
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

    // On and not stopped by a failure: a plugin left on after this machine failed to start it
    // carries the reason, and a dependant must not load against it.
    get running(): boolean {
        return this.enabled && this.loadError._ === undefined;
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

    // `undefined` hides the note shown.
    onUpdateNote(callback: (note: string | undefined) => void): () => void {
        this.noteListeners.add(callback);
        return () => {
            this.noteListeners.delete(callback);
        };
    }

    // The card first: a console a plugin made throw sends the error on to the lib's own handler.
    reportListenerError(registrar: string, err: unknown): void {
        this.runtimeError._ = `${registrar}: ${describeError(err)}`;
        console.error(`[${this.name}] Listener error in ${registrar}:`, err);
    }

    private note(text: string | undefined): void {
        for (const fn of this.noteListeners) {
            try {
                fn(text);
            } catch (e) {
                console.error(e);
            }
        }
    }

    // All actions follow: update local state -> persist to Rust DB -> route to Rust -> notify UI

    async load(): Promise<void> {
        // No-op: plugins are loaded into Rust at boot via jsrt.load_plugins.
        // This exists for API compatibility (upstream fromStorage calls it).
    }

    async install(): Promise<void> {
        // Read before Rust replaces the row: a failed start undoes only a first install;
        // a reinstall keeps what the user had, its stored data included.
        const wasInstalled = this.installed;
        this.acted = true;
        this.rowEpoch++;
        this.loading._ = true;
        try {
            // 1. Store (installed=1, enabled=0). Rust unloads a reinstalled plugin itself; a refused
            // or failed install throws here, before anything of this card changed.
            const info = await invokeIpc("plugin.install", this.url);
            this.applyUpdate(info);
            this.store.installed = true;
            this.name = this.store.package.name;
            if (wasInstalled) {
                // Rust unloaded the old code and wrote the row off: the card follows.
                this.stopReloadLoop();
                this.enabled = false;
                this.dropLoad();
            }
            LunaPlugin.plugins[this.url] = this;

            // 2. Enable atomically (Rust: check deps + DB enable + inject)
            try {
                await invokeIpc("plugin.enable", this.url);
                this.store.enabled = true;
                this.enabled = true;
            } catch (enableErr) {
                console.error(`[luna:plugin] Enable failed after install of '${this.name}':`, enableErr);
                if (wasInstalled || !(await this.undoInstall())) {
                    // Installed and visible, off until its row says otherwise
                    this.store.enabled = false;
                    this.enabled = false;
                    this.loadError._ = enableErr instanceof Error ? enableErr.message : String(enableErr);
                    void this.followRow();
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

    // Best-effort undo of a first install. The guard refuses while dependants exist; a
    // plugin never dispatched has its own cleanup. True when the row is gone.
    private async undoInstall(): Promise<boolean> {
        try {
            await invokeIpc("plugin.uninstall", this.url);
        } catch {
            try {
                await invokeIpc("plugin.cleanup_failed_install", this.url);
            } catch {
                return false;
            }
        }
        this.store.installed = false;
        delete LunaPlugin.plugins[this.url];
        syncUpstream(this.url, null);
        return true;
    }

    async enable(visited = new Set<string>()): Promise<void> {
        // Cycle = explicit error
        if (visited.has(this.url)) {
            console.error(`[luna:plugin] Circular dependency detected for '${this.name}'`);
            return;
        }
        visited.add(this.url);

        this.acted = true;
        this.rowEpoch++;
        this.loading._ = true;
        this.loadError._ = undefined; // Clear any previous error (e.g., timeout from last attempt)
        this.runtimeError._ = undefined;
        try {
            // Auto-activate disabled dependencies (recursive)
            for (const dep of this.dependencyRequirements) {
                const depPlugin = LunaPlugin.getByName(dep.name);
                if (!depPlugin || !depPlugin.store.installed) {
                    console.error(`[luna:plugin] Missing dependency '${dep.name}' for '${this.name}'`);
                    this.loadError._ = `Dependency '${dep.name}' is not installed`;
                    return;
                }
                depPlugin.addDependant(this);
                if (!depPlugin.running) {
                    await depPlugin.enable(visited);
                    if (!depPlugin.running) {
                        console.error(`[luna:plugin] Dependency '${dep.name}' failed to enable`);
                        this.loadError._ = `Dependency '${dep.name}' is not running`;
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
            // Off until the row says otherwise: Rust keeps the flag on when the failure was this
            // machine's, and only a plugin that failed by itself is turned off.
            this.enabled = false;
            // On the card, not only in the console: the switch falling back to off says nothing.
            this.loadError._ = e instanceof Error ? e.message : String(e);
            console.error(`[luna:plugin] Failed to enable '${this.name}':`, e);
            void this.followRow();
        } finally {
            this.loading._ = false;
        }
    }

    // The card shows the row an enable left. Only ever turns it on, and only while no lifecycle
    // step or write of `enabled` came since the read was asked; no answer leaves it off.
    private async followRow(): Promise<void> {
        const url = this.url;
        const epoch = this.rowEpoch;
        try {
            const rows = await invokeIpc("plugin.list");
            const row = Array.isArray(rows) ? rows.find((p: any) => p?.url === url) : undefined;
            if (row?.enabled && epoch === this.rowEpoch) this.enabled = true;
        } catch (e) {
            console.warn("[luna:plugin] plugin.list failed:", e);
        }
    }

    async disable(): Promise<void> {
        this.stopReloadLoop();
        this.acted = true;
        this.rowEpoch++;
        this.loading._ = true;
        try {
            // Atomic: guard dependants + unload + persist (Rust handles everything)
            await invokeIpc("plugin.disable", this.url);
            // Only after Rust confirms:
            this.enabled = false;
            this.dropLoad();
        } finally {
            this.loading._ = false;
        }
        // NO catch: errors propagate to caller (uninstall, UI)
    }

    // A restart's stop: Rust unloads the plugin and leaves its flag as the user set it. A start
    // refused next keeps the switch on with its reason, and the next load retries it.
    private async unload(): Promise<void> {
        this.acted = true;
        this.rowEpoch++;
        this.loading._ = true;
        try {
            await invokeIpc("plugin.unload", this.url);
            this.dropLoad();
        } finally {
            this.loading._ = false;
        }
        // NO catch: errors propagate to the restart
    }

    // What a stopped load leaves behind. Stopped on purpose: a failure reason must not outlive the
    // attempt it explains.
    private dropLoad(): void {
        this.loadError._ = undefined;
        this.runtimeError._ = undefined;
        this.exports = undefined;
        if ((window as any).__pluginExports) {
            delete (window as any).__pluginExports[this.url];
        }
        syncUpstream(this.url, this);
    }

    // Stores the version the plugin's url serves now, then restarts the plugin from what is
    // stored, unless another run holds it or it is gone. Core plugins have no url to fetch from.
    async reload(): Promise<void> {
        this.acted = true;
        this.loading._ = true;
        this.note(undefined);
        try {
            let pulled: Pulled | undefined;
            if (!LunaPlugin.corePlugins.has(this.name)) {
                pulled = await this.pullUpdate();
                this.note(pullNote(pulled));
                // Busy: the launch check, the live-reload poll or another Reload holds the plugin's
                // turn, and a restart now would run alongside theirs.
                if (pulled.outcome === "busy" || pulled.outcome === "gone") return;
            }
            await this.restart(pulled);
        } finally {
            this.loading._ = false;
        }
    }

    // Stores the source's version. Its hash and manifest are applied here rather than on
    // `jsrt.plugin_updated`: the poll compares against them on its next tick.
    private async pullUpdate(): Promise<Pulled> {
        let pulled: Pulled;
        try {
            pulled = (await invokeIpc("plugin.update", this.url)) ?? { outcome: "unavailable", reason: "no answer" };
        } catch (e) {
            pulled = { outcome: "unavailable", reason: e instanceof Error ? e.message : String(e) };
        }
        if (pulled.outcome === "updated") this.applyUpdate(pulled);
        return pulled;
    }

    // Unloads the plugin and starts it again from its stored version, its flag untouched: a plugin
    // that was not running starts, and a failed start is enable()'s loadError. Rust refuses the
    // unload while plugins depending on this one run, and it then runs as it was.
    private async restart(pulled: Pulled | undefined): Promise<void> {
        try {
            await this.unload();
        } catch (e) {
            if (pulled === undefined) {
                console.error(`[luna:plugin] Failed to reload '${this.name}':`, e);
            } else {
                this.note(stopRefusedNote(pulled, e instanceof Error ? e.message : String(e)));
            }
            return;
        }
        await this.enable();
    }

    // The card's Reload for every enabled plugin. One plugin's Reload after another fails: a
    // dependency's stop is refused while its dependant runs again. So each group tied by
    // dependencies stops dependants first, then starts dependencies first; a second call joins.
    static reloadAll(): Promise<ReloadAllResult> {
        LunaPlugin.reloadingAll ??= LunaPlugin.runReloadAll().finally(() => {
            LunaPlugin.reloadingAll = undefined;
        });
        return LunaPlugin.reloadingAll;
    }

    static get reloadAllInFlight(): Promise<ReloadAllResult> | undefined {
        return LunaPlugin.reloadingAll;
    }

    private static async runReloadAll(): Promise<ReloadAllResult> {
        const set = Object.values(LunaPlugin.plugins)
            .filter(p => p.installed && p.enabled && !LunaPlugin.corePlugins.has(p.name));
        const pulledOf = new Map<LunaPlugin, Pulled>();
        // A plugin whose epoch moved was acted on by someone else: the run leaves it as it is.
        const epochOf = new Map<LunaPlugin, number>();
        const left = new Map<LunaPlugin, string>();
        for (const p of set) {
            p.held = true;
            p.loading._ = true;
            p.acted = true;
            epochOf.set(p, p.rowEpoch);
        }
        try {
            for (const p of set) {
                p.note(undefined);
                const pulled = await p.pullUpdate();
                p.note(pullNote(pulled));
                pulledOf.set(p, pulled);
                if (pulled.outcome === "busy" || pulled.outcome === "gone") left.set(p, pullNote(pulled));
            }
            // After the pulls: a new version can declare a dependency, and Rust reads the stored one.
            const live = set.filter(p => !left.has(p));
            const byUrl = new Map(live.map(p => [p.url, p]));
            const groups = leafFirstGroups(live.map(p => ({
                url: p.url,
                name: p.name,
                dependencies: p.dependencyRequirements.map(d => d.name),
            })));
            // One group at a time: an unloaded plugin is not running until its start.
            for (const group of groups) {
                await LunaPlugin.restartGroup(group.map(url => byUrl.get(url)!), pulledOf, epochOf, left);
            }
        } finally {
            for (const p of set) {
                p.held = false;
                p.loading._ = false;
            }
        }
        // Read from the final state: a dependant's enable() can start a dependency once more.
        const result: ReloadAllResult = { reloaded: [], notReloaded: [] };
        for (const p of set) {
            const reason = left.get(p) ?? (p.running ? undefined : p.loadError._ ?? "Not running");
            if (reason === undefined) result.reloaded.push(p.name);
            else result.notReloaded.push({ name: p.name, reason });
        }
        return result;
    }

    private static async restartGroup(
        leafFirst: LunaPlugin[],
        pulledOf: Map<LunaPlugin, Pulled>,
        epochOf: Map<LunaPlugin, number>,
        left: Map<LunaPlugin, string>,
    ): Promise<void> {
        const stopped: LunaPlugin[] = [];
        try {
            for (const p of leafFirst) {
                if (!p.installed || !p.enabled || p.rowEpoch !== epochOf.get(p)) {
                    left.set(p, CHANGED_DURING_RELOAD);
                    continue;
                }
                try {
                    await p.unload();
                } catch (e) {
                    const note = stopRefusedNote(pulledOf.get(p)!, e instanceof Error ? e.message : String(e));
                    p.note(note);
                    left.set(p, note);
                    continue;
                } finally {
                    p.loading._ = true;
                }
                epochOf.set(p, p.rowEpoch);
                stopped.push(p);
            }
        } finally {
            for (const p of stopped.reverse()) {
                if (!p.installed || p.rowEpoch !== epochOf.get(p)) {
                    left.set(p, CHANGED_DURING_RELOAD);
                    continue;
                }
                await p.enable();
                p.loading._ = true;
            }
        }
    }

    // Takes in the row of a version Rust has just stored. Settings are left to the restarted
    // load's ready ack, which reads what that load registered.
    applyUpdate(info: any): void {
        try {
            const pkg = typeof info.manifest === "string" ? JSON.parse(info.manifest) : info.manifest;
            if (pkg?.name) this.store.package = pkg;
        } catch (e) { console.warn("[luna:plugin] Package info parse failed:", e); }
        if (info.hash) this.store.package.hash = info.hash;
        syncUpstream(this.url, this);
    }

    async uninstall(): Promise<void> {
        await this.disable();
        this.acted = true;
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

    // Rust moved the plugin to its store's new address: both registries and the card follow.
    moveTo(url: string): void {
        delete LunaPlugin.plugins[this.url];
        syncUpstream(this.url, null);
        this.url = url;
        this.store.url = url;
        LunaPlugin.plugins[url] = this;
        syncUpstream(url, this);
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

    // Each version the source serves is pulled once: any verdict on a hash settles it, and only a
    // change at the source asks again. A restart happens on `updated` alone; a source or machine
    // that fails is asked again later, the wait doubling up to POLL_MAX_MS.
    private startReloadLoop(): void {
        if (this.reloadTimeout) return;
        const loop = ++this.pollLoop;
        let failures = 0;

        // Neither a disable nor a Reload landed while the poll awaited: it may restart the plugin.
        const stillOurs = () => loop === this.pollLoop && this.enabled && !this.loading._ && !this.held;
        // A tick that a stop or a Reload overtook keeps quiet; its note would answer for them.
        const say = (text: string) => {
            if (stillOurs()) this.note(text);
        };

        const poll = async () => {
            if (loop !== this.pollLoop || !this.liveReload) return;
            let delay = POLL_MS;
            let failed = false;
            // A Reload in progress turns the plugin off for a moment: the next tick looks again.
            if (stillOurs()) {
                this.fetching._ = true;
                try {
                    const seen = (await invokeIpc("plugin.check_hash", this.url))?.hash;
                    if (seen && seen !== this.store.package.hash && seen !== this.seenHash) {
                        const pulled = await this.pullUpdate();
                        switch (pulled.outcome) {
                            case "updated":
                                this.seenHash = seen;
                                // Not ours any more: the stored version loads at the next enable.
                                if (stillOurs()) await this.restart(pulled);
                                break;
                            case "upToDate":
                                this.seenHash = seen;
                                break;
                            // Noted: the card is the one place these reasons reach the user.
                            case "refused":
                                this.seenHash = seen;
                                say(pullNote(pulled));
                                break;
                            case "blocked":
                                delay = POLL_MAX_MS;
                                say(pullNote(pulled));
                                break;
                            case "unavailable":
                                failed = true;
                                say(pullNote(pulled));
                                break;
                            case "busy":
                                break;
                            case "gone":
                                this.stopReloadLoop();
                                return;
                        }
                    }
                } catch (e) {
                    console.error(`[luna:plugin] Live reload error for ${this.name}:`, e);
                    failed = true;
                    // Only `plugin.check_hash` rejects here: no update was tried.
                    say(`Could not check the source: ${e instanceof Error ? e.message : String(e)}`);
                } finally {
                    this.fetching._ = false;
                }
            }
            // Consecutive failures only: one tick that went through starts the count again.
            failures = failed ? failures + 1 : 0;
            if (failed) delay = Math.min(POLL_MS * 2 ** failures, POLL_MAX_MS);
            if (loop === this.pollLoop && this.liveReload) {
                this.reloadTimeout = setTimeout(poll, delay);
            }
        };

        this.reloadTimeout = setTimeout(poll, POLL_MS);
    }

    private stopReloadLoop(): void {
        this.pollLoop++;
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

    // The card already shown for this url installs itself: a reinstall keeps the object the UI
    // holds; a new url gets one registered only once its install went through.
    static async install(url: string): Promise<LunaPlugin> {
        const cleanUrl = url.replace(/(\.(mjs|json|mjs\.map))$/, "");
        const plugin = LunaPlugin.plugins[cleanUrl] ?? new LunaPlugin({
            url: cleanUrl,
            package: { name: nameFromUrl(cleanUrl) },
            enabled: false,
            installed: false,
            liveReload: false,
            hideSettings: true,
        });
        await plugin.install();
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
                pkg = { name: nameFromUrl(url) };
            }
        }

        // A move or an install may have registered this url during the fetch: that object stays the
        // one, or this placeholder would take the installed plugin's place in the registry.
        const raced = LunaPlugin.plugins[url];
        if (raced) return raced;

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
