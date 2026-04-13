// Module registry - maps module names (e.g. "@luna/core") to their exports
export const modules: Record<string, any> = {};

// --- Tracer: console wrapper with plugin name prefix ---
// Called as a function: const { trace, errSignal } = Tracer("[PluginName]")

export function Tracer(name: string): { trace: (...args: any[]) => void; errSignal: (...args: any[]) => void } {
    const prefix = name.startsWith("[") ? name : `[${name}]`;
    return {
        trace: (...args: any[]) => console.log(prefix, ...args),
        errSignal: (...args: any[]) => console.error(prefix, ...args),
    };
}

// --- StyleTag: managed <style> element ---
// API matches TidaLuna: new StyleTag(name, unloads), .css setter, .remove()

export class StyleTag {
    private el: HTMLStyleElement;
    constructor(name?: string, unloads?: Set<() => void>) {
        this.el = document.createElement("style");
        if (name) this.el.dataset.luna = name;
        document.head.appendChild(this.el);
        if (unloads) unloads.add(() => this.remove());
    }
    get css(): string | undefined {
        return this.el.textContent || undefined;
    }
    set css(value: string | undefined) {
        this.el.textContent = value ?? "";
    }
    remove() {
        this.el.remove();
    }
}

// --- observe: minimal oby-compatible reactive shim ---

export function observe<T>(initial: T): [() => T, (value: T) => void] {
    let current = initial;
    return [() => current, (value: T) => { current = value; }];
}

// --- unloadSet: execute all cleanup functions in a Set, then clear it ---
// API matches TidaLuna: unloadSet(set) drains cleanup functions

export function unloadSet(set: Set<() => void>): void {
    for (const fn of set) {
        try { fn(); } catch (e) { console.error("[luna:unloadSet]", e); }
    }
    set.clear();
}

// --- ftch: typed fetch wrapper ---
// API matches TidaLuna: ftch.json<T>(url, options?), ftch.text(url, options?)

export const ftch = {
    async json<T = any>(url: string, options?: RequestInit): Promise<T> {
        const res = await fetch(url, options);
        if (!res.ok) throw new Error(`ftch.json: ${res.status} ${res.statusText}`);
        return res.json();
    },
    async text(url: string, options?: RequestInit): Promise<string> {
        const res = await fetch(url, options);
        if (!res.ok) throw new Error(`ftch.text: ${res.status} ${res.statusText}`);
        return res.text();
    },
};

// --- Messager: toast notification system ---
// Stub - logs to console. A real toast UI can be added later.

export const Messager = {
    Info(...args: any[]) {
        console.log("[luna:info]", ...args);
    },
    Error(...args: any[]) {
        console.error("[luna:error]", ...args);
    },
};

// --- SettingsTransfer: stub for settings import/export ---

export type ExportData = {
    timestamp: number;
    featureFlags?: Record<string, boolean> | null;
    stores?: Record<string, any>;
    plugins?: any[];
};

export const SettingsTransfer = {
    async dump(_stripCode: boolean, _featureFlags: Record<string, boolean> | null): Promise<ExportData> {
        console.warn("[luna:SettingsTransfer] dump() not yet implemented in TidaLunar");
        return { timestamp: Date.now() };
    },
    validate(data: any): data is ExportData {
        return data && typeof data === "object" && typeof data.timestamp === "number";
    },
    async restore(_data: ExportData): Promise<void> {
        console.warn("[luna:SettingsTransfer] restore() not yet implemented in TidaLunar");
    },
};

// --- Setup: expose window.luna and window.require ---

export function setupCore() {
    const luna = {
        core: {
            modules,
            Tracer,
            StyleTag,
            unloadSet,
            redux: null as any,
            LunaPlugin: { plugins: {} as Record<string, any> },
        },
    };

    (window as any).luna = luna;
    (window as any).require = (name: string) => {
        const mod = modules[name];
        if (!mod) {
            console.warn(`[luna] Module not found: ${name}`);
        }
        return mod;
    };

    return luna;
}
