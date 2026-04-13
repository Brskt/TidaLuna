import { invokeIpc, onIpcEvent } from "../ipc";
import { LunaPlugin, setUpstreamPlugins } from "./luna-plugin";
import { LunaPlugin as CoreLunaPlugin, modules } from "../../render/src";

// Redirect ALL upstream LunaPlugin lifecycle to our LunaPlugin (Rust IPC).
// The upstream class is used by @luna/ui for display - its static methods
// and prototype methods must route through our system, not execute in CEF.

// Static: fromStorage is the main entry point used by Plugin Store UI.
// Return our LunaPlugin instances so all subsequent method calls (install,
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

// Wire the upstream registry so lifecycle methods can sync both registries.
setUpstreamPlugins(CoreLunaPlugin.plugins as any);

// Expose management API on window.luna.core for devtools console / plugin use
export function exposeLoaderApi() {
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
    // Maps core plugin name → subdirectory in Inrixia/TidaLuna/plugins/
    const corePluginPaths: Record<string, string> = {
        "@luna/lib.native": "lib.native",
        "@luna/lib": "lib",
        "@luna/ui": "ui",
        "@luna/dev": "dev",
    };
    for (const [name, dir] of Object.entries(corePluginPaths)) {
        if (LunaPlugin.plugins[name]) continue;
        // Create plugin with name-only manifest immediately so it shows up in the UI
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

    // Resync frontend on plugin lifecycle events from Rust
    onIpcEvent("jsrt.plugin_failed", (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin) {
            plugin.enabled = false;
            plugin.store.enabled = false;
            plugin.loadError._ = "Plugin failed to initialize (timeout)";
            window.dispatchEvent(new Event("luna:plugins-updated"));
        }
    });
    onIpcEvent("jsrt.plugin_ready_confirmed", (url: string) => {
        const plugin = LunaPlugin.getByUrl(url);
        if (plugin && plugin.loadError._) {
            plugin.loadError._ = undefined;
            window.dispatchEvent(new Event("luna:plugins-updated"));
        }
    });

    // Populate plugin registry from Rust PluginStore.
    // Plugins execute in Rust, but the UI reads LunaPlugin.plugins for display.
    invokeIpc("plugin.list").then((plugins: any[]) => {
        console.log("[luna:loader] plugin.list returned:", plugins?.length, "plugins");
        if (!Array.isArray(plugins)) return;
        for (const p of plugins) {
            if (!LunaPlugin.plugins[p.url]) {
                const plugin = LunaPlugin.fromPluginInfo(p);
                (CoreLunaPlugin.plugins as any)[p.url] = plugin;
                // Extract Settings in background for enabled plugins
                if (plugin.enabled && plugin.installed) {
                    plugin.extractSettings().then(() => {
                        window.dispatchEvent(new Event("luna:plugins-updated"));
                    });
                }
            }
        }
        console.log("[luna:loader] LunaPlugin.plugins:", Object.keys(LunaPlugin.plugins).length, "total");
        window.dispatchEvent(new Event("luna:plugins-updated"));
    }).catch((e) => console.warn("[luna:loader] plugin.list failed:", e));
}
