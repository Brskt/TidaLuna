import { invokeIpc } from "../ipc";
import { LunaPlugin } from "./luna-plugin";

// Expose management API on window.luna.core for devtools console / plugin use
export function exposeLoaderApi() {
    const luna = (window as any).luna;
    if (!luna) return;

    luna.core.installPlugin = async (url: string) => {
        const plugin = await LunaPlugin.install(url);
        console.log("[luna] Installed:", plugin.name);
        return plugin;
    };

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
}
