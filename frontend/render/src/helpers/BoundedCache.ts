/**
 * Map-backed LRU cache with a size cap, pin refcounting, and an eviction hook.
 *
 * `get` re-touches recency; `set` evicts the least-recently-used *unpinned* entry once
 * `size` exceeds `maxSize`. Pinned entries are never evicted by the cap (a caller that must
 * keep an entry alive pins it). `onEvict` fires exactly once per removal (cap eviction,
 * `delete`, `clear`, or replacement) so owners can release resources tied to the value.
 *
 * `maxSize = Infinity` disables cap eviction: the cache is unbounded but still disposes on
 * `delete`/`clear`.
 */
export class BoundedCache<K, V> {
	private readonly map = new Map<K, V>();
	private readonly pins = new Map<K, number>();

	constructor(
		private readonly maxSize: number,
		private readonly onEvict?: (key: K, value: V) => void,
	) {}

	public get size(): number {
		return this.map.size;
	}

	public has(key: K): boolean {
		return this.map.has(key);
	}

	/** Read without changing recency. */
	public peek(key: K): V | undefined {
		return this.map.get(key);
	}

	/** Read and mark most-recently-used. */
	public get(key: K): V | undefined {
		if (!this.map.has(key)) return undefined;
		const value = this.map.get(key)!;
		this.map.delete(key);
		this.map.set(key, value);
		return value;
	}

	public set(key: K, value: V): void {
		const hadOld = this.map.has(key);
		const old = hadOld ? this.map.get(key)! : undefined;
		this.map.delete(key);
		this.map.set(key, value);
		if (hadOld && old !== value) this.onEvict?.(key, old as V);
		this.evictOverflow(key);
	}

	/** Force-remove regardless of pin state; fires `onEvict`. */
	public delete(key: K): boolean {
		if (!this.map.has(key)) return false;
		const value = this.map.get(key)!;
		this.map.delete(key);
		this.pins.delete(key);
		this.onEvict?.(key, value);
		return true;
	}

	public clear(): void {
		for (const [key, value] of this.map) this.onEvict?.(key, value);
		this.map.clear();
		this.pins.clear();
	}

	/** Prevent eviction while at least one pin is held. */
	public pin(key: K): void {
		this.pins.set(key, (this.pins.get(key) ?? 0) + 1);
	}

	public unpin(key: K): void {
		const n = this.pins.get(key) ?? 0;
		if (n <= 1) this.pins.delete(key);
		else this.pins.set(key, n - 1);
	}

	private evictOverflow(protect?: K): void {
		if (this.map.size <= this.maxSize) return;
		// Map preserves insertion order, so keys() yields least-recently-used first.
		for (const key of this.map.keys()) {
			if (this.map.size <= this.maxSize) break;
			// Never evict a pinned entry, nor the key just inserted by set(): scanning past pinned
			// entries must not dispose the value we are about to return. The cache may exceed maxSize
			// by one, exactly as it does when every entry is pinned.
			if (key === protect || (this.pins.get(key) ?? 0) > 0) continue;
			const value = this.map.get(key)!;
			this.map.delete(key);
			this.onEvict?.(key, value);
		}
	}
}
