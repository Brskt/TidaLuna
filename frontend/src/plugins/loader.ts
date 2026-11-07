import { invokeIpc, onIpcEvent } from "../ipc";
import { LunaPlugin, setUpstreamPlugins } from "./luna-plugin";
import { LunaPlugin as CoreLunaPlugin, modules } from "../../render/src";

// Redirect ALL upstream LunaPlugin lifecycle to our LunaPlugin (Rust IPC).
// The upstream class is used by @luna/ui for display - its static methods
// and prototype methods must route through our system, not execute in CEF.

// Static: fromStorage is the main entry point used by Plugin Store UI.
// Return our LunaPlugin instances: all subsequent method calls (install,
// enable, etc.) go through our class which routes to Rust via IPC.
CoreLunaPlugin.fromStorage = async function (storeInit: any): Promise<any> {
    return LunaPlugin.fromStorage(storeInit);
};

// Prototype overrides - safety net in case an upstream instance is created
// through a path we didn't intercept. Each finds our instance by URL and delegates.
CoreLunaPlugin.prototype.load = async function () { return this; };

CoreLunaPlugin.prototype.install = async function () {
    const ours = LunaPlugin.getByUrl(this.url) ?? await LunaPlugin.install(this.url);
    if (ours && !ours.installed) await ours.install();
};

CoreLunaPlugin.prototype.enable = async function () {
    const ours = LunaPlugin.getByUrl(this.url);
    if (ours) await ours.enable();
};

CoreLunaPlugin.prototype.disable = async function () {
    const ours = LunaPlugin.getByUrl(this.url);
    if (ours) await ours.disable();
};

CoreLunaPlugin.prototype.uninstall = async function () {
    const ours = LunaPlugin.getByUrl(this.url);
    if (ours) await ours.uninstall();
};

CoreLunaPlugin.prototype.reload = async function () {
    const ours = LunaPlugin.getByUrl(this.url);
    if (ours) await ours.reload();
};

// Upstream imports plugin code into the page, past every check Rust makes; no instance may do that.
(CoreLunaPlugin.prototype as any).loadExports = async function () {
    throw new Error("Plugins run through the host, never imported into the page");
};

// Wire the upstream registry for lifecycle methods to sync both registries.
setUpstreamPlugins(CoreLunaPlugin.plugins as any);

// The urls the early fill put in the registry: the only ones its reconciliation may take out.
const filledEarly = new Set<string>();
let early: Promise<void> | undefined;

function adopt(row: any): LunaPlugin {
    const plugin = LunaPlugin.fromPluginInfo(row);
    (CoreLunaPlugin.plugins as any)[row.url] = plugin;
    return plugin;
}

/**
 * The registry and its lifecycle events, before `@luna/ui` loads: a Store card made earlier would
 * show an installed plugin as not installed, and its Install would reinstall it over the user's.
 * Once per page; a failed list leaves the cards to the reconciliation in `exposeLoaderApi`.
 */
export function loadRegistry(): Promise<void> {
    early ??= (async () => {
        registerLifecycleEvents();
        try {
            const plugins = await invokeIpc("plugin.list");
            if (!Array.isArray(plugins)) return;
            for (const p of plugins) {
                if (LunaPlugin.plugins[p.url]) continue;
                adopt(p);
                filledEarly.add(p.url);
            }
        } catch (e) {
            console.warn("[luna:loader] plugin.list failed:", e);
        }
    })();
    return early;
}

/** A move the rows have not confirmed yet; events for its new url wait in `queue`. */
type PendingMove = { sources: Set<string>; pending: number; queue: Array<() => void> };
const moving = new Map<string, PendingMove>();

// A page script may have replaced console: a throw here would leave a move held for good.
function warn(...args: unknown[]) {
    try {
        console.warn(...args);
    } catch {}
}

// Rust emits a moved plugin's events right after `plugin_moved`, one eval each and in that order,
// but the page takes the move only once the rows show it: held until then, they reach the moved
// plugin. A `plugin_updated` from an update check is not ordered with the move.
function onPluginEvent(channel: string, handler: (url: string, ...args: any[]) => void) {
    onIpcEvent(channel, (url: string, ...args: any[]) => {
        const move = moving.get(url);
        if (move) move.queue.push(() => handler(url, ...args));
        else handler(url, ...args);
    });
}

// One check of a move is done: the last one lets the held events through, in the order they came.
function release(to: string, from: string, move: PendingMove) {
    move.sources.delete(from);
    if (--move.pending > 0) return;
    if (moving.get(to) === move) moving.delete(to);
    for (const replay of move.queue) {
        try {
            replay();
        } catch (e) {
            warn("[luna:loader] plugin event failed:", e);
        }
    }
}

// Resync frontend on plugin lifecycle events from Rust
function registerLifecycleEvents() {
    // The reason is shown as text, whoever emitted it; without one, the ready ack timed out.
    onPluginEvent("jsrt.plugin_failed", (url: string, reason?: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) {
            plugin.enabled = false;
            plugin.store.enabled = false;
            // Its code is unloaded: the Settings it registered must not stay on the card.
            plugin.exports = undefined;
            plugin.loadError._ = typeof reason === "string" ? reason : "Plugin failed to initialize (timeout)";
            window.dispatchEvent(new Event("luna:plugins-updated"));
        }
    });
    // Rust unloaded the plugin and could not start it again this session; its flag is untouched.
    onPluginEvent("jsrt.plugin_stopped", (url: string, reason?: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (!plugin) return;
        plugin.exports = undefined;
        plugin.loadError._ = typeof reason === "string" ? reason : "Plugin stopped";
        window.dispatchEvent(new Event("luna:plugins-updated"));
    });
    onPluginEvent("jsrt.plugin_ready_confirmed", (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (!plugin) return;
        // The plugin's top level has finished: its Settings, if any, are registered now.
        const found = plugin.extractSettings();
        // A restarted version without Settings must not keep showing its predecessor's.
        const dropped = !found && plugin.exports !== undefined;
        if (dropped) plugin.exports = undefined;
        if (found || dropped || plugin.loadError._) {
            plugin.loadError._ = undefined;
            window.dispatchEvent(new Event("luna:plugins-updated"));
        }
    });

    // Any page script can call the emitter: the url only picks the plugin, the row is read again.
    onPluginEvent("jsrt.plugin_updated", (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (!plugin) return;
        invokeIpc("plugin.list").then((plugins: any[]) => {
            const info = Array.isArray(plugins) ? plugins.find(p => p.url === url) : undefined;
            if (info) plugin.applyUpdate(info);
        }).catch((e) => console.warn("[luna:loader] plugin.list failed:", e));
    });

    // Any page script can call the emitter: the move is taken once the rows show it, only for an
    // installed plugin (a Store placeholder is not one) and never onto one. Strings only: a registry
    // lookup coerces its key, the rows do not. A pair already being checked is not checked again.
    onIpcEvent("jsrt.plugin_moved", (from: unknown, to: unknown) => {
        if (typeof from !== "string" || typeof to !== "string") return;
        const plugin = LunaPlugin.getByUrl(from);
        if (!plugin || plugin.url !== from) return;
        let move = moving.get(to);
        if (move?.sources.has(from)) return;
        if (!move) {
            move = { sources: new Set(), pending: 0, queue: [] };
            moving.set(to, move);
        }
        move.sources.add(from);
        move.pending++;
        const checked = move;
        invokeIpc("plugin.list").then((plugins: any[]) => {
            if (!Array.isArray(plugins)) return;
            const urls = new Set(plugins.map(p => p.url));
            const confirmed = urls.has(to) && !urls.has(from) && plugin.installed && !LunaPlugin.plugins[to]?.installed;
            if (confirmed && LunaPlugin.getByUrl(from) === plugin) plugin.moveTo(to);
        }).catch((e) => warn("[luna:loader] plugin move not taken:", e))
            .finally(() => release(to, from, checked));
    });
}

/**
 * Bring the registry to the rows the startup pass left: it turns plugins off and drops duplicate
 * rows without an event. An instance the user acted on keeps what Rust answered that action, which
 * a row read before it would undo.
 */
function reconcile(plugins: any[]) {
    const rows = new Set<string>();
    for (const p of plugins) {
        rows.add(p.url);
        const existing = LunaPlugin.plugins[p.url];
        const plugin = existing ?? adopt(p);
        if (existing && !existing.acted && !existing.loading._) {
            existing.store.installed = !!p.installed;
            existing.enabled = !!p.enabled;
            if (p.loadError) existing.loadError._ = p.loadError;
            existing.applyUpdate(p);
        } else if (existing?.installed) {
            (CoreLunaPlugin.plugins as any)[p.url] = existing;
        }
        // Already ready before this page listened: the ready handler missed it.
        if (plugin.enabled && plugin.installed) plugin.extractSettings();
    }
    for (const url of filledEarly) {
        const plugin = LunaPlugin.plugins[url];
        if (rows.has(url) || !plugin || plugin.acted || plugin.loading._) continue;
        delete LunaPlugin.plugins[url];
        delete (CoreLunaPlugin.plugins as any)[url];
    }
}

// Expose management API on window.luna.core for devtools console / plugin use
export function exposeLoaderApi(): Promise<void> | undefined {
    const luna = (window as any).luna;
    if (!luna) return;

    luna.core.installPlugin = (url: string) => LunaPlugin.install(url);
    luna.core.uninstallPlugin = async (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) await plugin.uninstall();
    };
    luna.core.enablePlugin = async (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) await plugin.enable();
    };
    luna.core.disablePlugin = async (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) await plugin.disable();
    };
    luna.core.reloadPlugin = async (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) await plugin.reload();
    };

    luna.core.listPlugins = () => invokeIpc("plugin.list");
    luna.core.loadedPlugins = LunaPlugin.plugins;

    // Register core plugins - fetch manifests from upstream GitHub repo.
    // Maps core plugin name -> subdirectory in Inrixia/TidaLuna/plugins/
    const corePluginPaths: Record<string, string> = {
        "@luna/lib.native": "lib.native",
        "@luna/lib": "lib",
        "@luna/ui": "ui",
        "@luna/dev": "dev",
    };
    for (const [name, dir] of Object.entries(corePluginPaths)) {
        if (LunaPlugin.plugins[name]) continue;
        // Create plugin with name-only manifest immediately: it shows up in the UI
        const plugin = LunaPlugin.fromPluginInfo({
            url: name,
            name,
            manifest: { name, luna: { type: "library" } },
            enabled: true,
            installed: true,
        });
        const mod = modules[name];
        if (mod?.Settings) {
            plugin.exports = { Settings: mod.Settings };
        }
        (CoreLunaPlugin.plugins as any)[name] = plugin;
        // Fetch full manifest in background - updates description/author when ready
        const url = `https://raw.githubusercontent.com/Inrixia/TidaLuna/master/plugins/${dir}/package.json`;
        fetch(url).then(r => r.json()).then((pkg: any) => {
            if (pkg?.name) {
                plugin.store.package = { ...plugin.store.package, ...pkg };
                plugin.name = pkg.name;
                window.dispatchEvent(new Event("luna:plugins-updated"));
            }
        }).catch(() => {});
    }

    // The registry after the startup pass. Plugins execute in Rust, but the UI reads
    // LunaPlugin.plugins for display.
    return invokeIpc("plugin.list").then((plugins: any[]) => {
        console.log("[luna:loader] plugin.list returned:", plugins?.length, "plugins");
        if (!Array.isArray(plugins)) return;
        reconcile(plugins);
        console.log("[luna:loader] LunaPlugin.plugins:", Object.keys(LunaPlugin.plugins).length, "total");
        window.dispatchEvent(new Event("luna:plugins-updated"));
        // After the listeners and the registry: an update landing earlier would reach neither.
        // Rust runs it once per launch, if its setting allows.
        invokeIpc("plugin.update_all").catch((e) => console.warn("[luna:loader] plugin.update_all failed:", e));
    }).catch((e) => console.warn("[luna:loader] plugin.list failed:", e));
}
