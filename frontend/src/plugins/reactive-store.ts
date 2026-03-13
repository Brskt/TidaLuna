import { invokeIpc } from "../ipc";
import { store as obyStore } from "oby";

export class ReactiveStore {
    private static instances = new Map<string, ReactiveStore>();
    private namespace: string;
    private cache = new Map<string, any>();

    constructor(namespace: string) {
        this.namespace = namespace;
    }

    /**
     * Get or create a ReactiveStore for a namespace.
     * API matches TidaLuna: ReactiveStore.getStore("@luna/storage")
     */
    static getStore(namespace: string): ReactiveStore {
        let instance = ReactiveStore.instances.get(namespace);
        if (!instance) {
            instance = new ReactiveStore(namespace);
            ReactiveStore.instances.set(namespace, instance);
        }
        return instance;
    }

    async get(key: string): Promise<any> {
        if (this.cache.has(key)) return this.cache.get(key);
        const raw = await invokeIpc("plugin.storage.get", this.namespace, key);
        if (raw != null) {
            try {
                const value = JSON.parse(raw);
                this.cache.set(key, value);
                return value;
            } catch (e) {
                console.warn("[luna:store] Reactive store error:", e);
                return raw;
            }
        }
        return undefined;
    }

    async set(key: string, value: any): Promise<void> {
        this.cache.set(key, value);
        await invokeIpc(
            "plugin.storage.set",
            this.namespace,
            key,
            JSON.stringify(value),
        );
    }

    async delete(key: string): Promise<void> {
        this.cache.delete(key);
        await invokeIpc("plugin.storage.del", this.namespace, key);
    }

    async keys(): Promise<string[]> {
        return await invokeIpc("plugin.storage.keys", this.namespace);
    }

    /**
     * Get a reactive value backed by SQLite persistence.
     * Returns an oby store()-wrapped value. Mutations are observed and auto-persisted.
     * API matches TidaLuna: store.getReactive<T>(key, defaultValue)
     */
    async getReactive<T>(key: string, defaultValue: T): Promise<T> {
        const stored = await this.get(key);
        const initial = stored !== undefined ? stored : defaultValue;

        // Wrap in oby.store() for deep reactivity
        const reactive = obyStore(initial);

        // Observe changes and persist to SQLite
        obyStore.on(reactive, () => {
            const raw = obyStore.unwrap(reactive);
            this.set(key, raw);
        });

        return reactive;
    }

    // TidaLuna API: returns a Proxy where reads are sync (cached)
    // and writes persist to SQLite asynchronously.
    static async getPluginStorage<T extends Record<string, any>>(
        namespace: string,
        defaults: T,
    ): Promise<T> {
        const store = ReactiveStore.getStore(namespace);

        // Load existing stored values
        const storedKeys = await store.keys();
        const stored: Record<string, any> = {};
        if (Array.isArray(storedKeys)) {
            for (const key of storedKeys) {
                stored[key] = await store.get(key);
            }
        }

        // Merge: stored values override defaults
        const data: any = { ...defaults, ...stored };

        return new Proxy(data, {
            set(_target, prop: string, value) {
                _target[prop] = value;
                store.set(prop, value);
                return true;
            },
        }) as T;
    }
}
