import type { AnyRecord, MaybePromise } from "@inrixia/helpers";
import { createStore as createIdbStore, del as idbDel, get as idbGet, keys as idbKeys, set as idbSet, type UseStore } from "idb-keyval";
import { store as obyStore } from "oby";
import { BoundedCache } from "./helpers/BoundedCache";
import { coreTrace, type Tracer } from "./trace";

type StoreReconcileable = AnyRecord | any[];

export class ReactiveStore {
	public static Storages: Record<string, ReactiveStore> = {};
	public static async getPluginStorage<T extends AnyRecord>(pluginName: string, defaultValue?: T) {
		const pluginStore = this.getStore(`@luna/pluginStorage`);
		const storage = await pluginStore.getReactive<T>(pluginName);
		if (defaultValue !== undefined) Object.keys(defaultValue).forEach((key) => (storage[key as keyof T] ??= defaultValue[key]));
		return storage;
	}
	public static getStore(name: string, maxSize: number = Infinity): ReactiveStore {
		return (this.Storages[name] ??= new this(name, maxSize));
	}

	public readonly idbStore: UseStore;
	public readonly trace: Tracer;
	// Bounded per store (opt-in via getStore's maxSize; default unbounded). Disposes each entry's
	// idb-write subscription when it leaves the cache (LRU eviction, del, or clear) since oby cannot
	// clean the listener up via GC. Bounding lives here, decoupled from the instances that hold the
	// reactive object: an instance evicted from ContentBase._instances keeps a working, still-
	// subscribed reactive object for as long as this cache retains it (and shares it on re-fetch).
	private readonly reactiveCache: BoundedCache<string, { store: any; dispose: () => void }>;
	private constructor(
		public readonly idbName: string,
		maxSize: number = Infinity,
	) {
		this.trace = coreTrace.withSource(`.ReactiveStore[${idbName}]`).trace;
		this.idbStore = createIdbStore(idbName, "_");
		this.reactiveCache = new BoundedCache(maxSize, (key, entry) => {
			entry.dispose();
			// Gated on the mirrored Rust log level (>= 2, like sendDbgIpc): surfaces evictions in the
			// [JS] logs so the bound is observable, and stays silent (unemitted) in normal use.
			if (Number((window as any).__TIDALUNAR_LOG_LEVEL__ ?? 0) >= 2)
				this.trace.log(`evicted "${key}" - reactiveCache holds ${this.reactiveCache.size}`);
		});
	}
	public async getReactive<T extends StoreReconcileable>(key: string, defaultValue: T = <T>{}): Promise<T> {
		const cached = this.reactiveCache.get(key);
		if (cached !== undefined) return cached.store as T;

		// Create the oby reactive object
		const reactiveObj = obyStore(defaultValue);
		// Reconcile the object with the idb store to ensure we have the latest values
		obyStore.reconcile(reactiveObj, (await idbGet<T>(key, this.idbStore)) ?? defaultValue);

		// A concurrent getReactive for the same key may have populated the entry during the
		// await above; if so use that winner and drop our redundant reactive object.
		const winner = this.reactiveCache.get(key);
		if (winner !== undefined) return winner.store as T;

		// Set up a listener to write to the idb store when the object changes, keeping its
		// disposer so eviction can detach it (oby cannot clean this listener up via GC).
		const dispose = obyStore.on(reactiveObj, () =>
			idbSet(key, obyStore.unwrap(reactiveObj), this.idbStore).catch(this.trace.err.withContext(`Failed to set`, key, reactiveObj)),
		);

		this.reactiveCache.set(key, { store: reactiveObj, dispose });
		return reactiveObj;
	}

	public async ensure<T>(key: string, defaultValue: T | (() => MaybePromise<T>), awaitSet: boolean = false): Promise<T> {
		const value = await this.get<T>(key);
		if (value === undefined) {
			// Reduce defaultValue to its actual value
			defaultValue = defaultValue instanceof Function ? await defaultValue() : defaultValue;
			const setPromise = this.set(key, defaultValue).catch(this.trace.err.withContext(`Failed to set`, key, defaultValue));
			// Only wait for set to complete if awaitSet is true
			if (awaitSet) await setPromise;
			return defaultValue;
		}
		return value;
	}

	public get<T>(key: string): Promise<T | undefined> {
		return idbGet<T>(key, this.idbStore);
	}

	public async set<T>(key: string, value: T) {
		try {
			await idbSet(key, value, this.idbStore);
			const entry = this.reactiveCache.peek(key);
			if (entry !== undefined) {
				// Reconcile the reactive object with the new value
				obyStore.reconcile(entry.store, value);
			}
			return value;
		} catch (err) {
			this.trace.err.withContext(`Failed to set`, key, value)(err);
			throw err;
		}
	}

	public del(key: string) {
		// Detach the in-memory reactive object + its subscription before removing from idb.
		this.reactiveCache.delete(key);
		return idbDel(key, this.idbStore);
	}

	public keys(): Promise<string[]> {
		return idbKeys(this.idbStore);
	}

	public async dump(): Promise<Record<string, unknown>>
	{
		const allKeys = await this.keys();
		const data: Record<string, unknown> = {};
		for (const key of allKeys)
			data[key] = await this.get(key);

		return data;
	}

	public async clear()
	{
		// Detach every in-memory reactive object + subscription, then clear idb.
		this.reactiveCache.clear();
		const allKeys = await this.keys();
		for (const key of allKeys)
			await idbDel(key, this.idbStore);
	}
}
