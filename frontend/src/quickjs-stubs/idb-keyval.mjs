/**
 * idb-keyval shim for rquickjs runtime.
 *
 * Bridges idb-keyval's API to Rust PluginStore SQLite via globals:
 *   __storage_get(namespace, key) → string | undefined
 *   __storage_set(namespace, key, value) → void
 *   __storage_del(namespace, key) → void
 *   __storage_keys(namespace) → string (JSON array)
 *
 * Values are JSON-serialized for SQLite TEXT storage.
 */

// UseStore is just a namespace identifier in our shim
export function createStore(dbName, _storeName) {
    return dbName;
}

export function get(key, store) {
    const ns = store ?? "_default";
    const raw = __storage_get(ns, key);
    if (raw === undefined || raw === null) return Promise.resolve(undefined);
    try {
        return Promise.resolve(JSON.parse(raw));
    } catch {
        return Promise.resolve(raw);
    }
}

export function set(key, value, store) {
    const ns = store ?? "_default";
    const raw = JSON.stringify(value);
    __storage_set(ns, key, raw);
    return Promise.resolve();
}

export function del(key, store) {
    const ns = store ?? "_default";
    __storage_del(ns, key);
    return Promise.resolve();
}

export function keys(store) {
    const ns = store ?? "_default";
    const raw = __storage_keys(ns);
    try {
        return Promise.resolve(JSON.parse(raw));
    } catch {
        return Promise.resolve([]);
    }
}

export function update(key, updater, store) {
    return get(key, store).then((val) => {
        return set(key, updater(val), store);
    });
}

export function getMany(keys_, store) {
    return Promise.all(keys_.map((k) => get(k, store)));
}

export function setMany(entries, store) {
    return Promise.all(entries.map(([k, v]) => set(k, v, store))).then(() => {});
}

export function delMany(keys_, store) {
    return Promise.all(keys_.map((k) => del(k, store))).then(() => {});
}

export function clear(store) {
    return keys(store).then((ks) => delMany(ks, store));
}

export function entries(store) {
    return keys(store).then((ks) =>
        Promise.all(ks.map((k) => get(k, store).then((v) => [k, v])))
    );
}

export function values(store) {
    return keys(store).then((ks) => Promise.all(ks.map((k) => get(k, store))));
}
