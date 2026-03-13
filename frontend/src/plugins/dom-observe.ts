// --- observe / observePromise: DOM element observation via shared MutationObserver ---
// Matches upstream @luna/lib helpers/observable.ts API

type ObserveCallback<E extends Element = Element> = (elem: E) => unknown;
type ObserveEntry = [selector: string, callback: ObserveCallback<Element>];

const observables = new Set<ObserveEntry>();
const sharedObserver = new MutationObserver((records) => {
    if (observables.size === 0) return;
    const seenElems = new Set<Node>();
    for (const record of records) {
        const elem = record.target;
        if (elem.nodeType !== Node.ELEMENT_NODE || seenElems.has(elem)) continue;
        seenElems.add(elem);
        for (const obs of observables) {
            const [sel, cb] = obs;
            if ((elem as Element).matches(sel)) cb(elem as Element);
            for (const el of (elem as Element).querySelectorAll(sel)) cb(el);
        }
    }
});

export function observe<T extends Element = Element>(
    unloads: Set<() => void>,
    selector: string,
    cb: ObserveCallback<T>,
): () => void {
    if (observables.size === 0) {
        sharedObserver.observe(document.body, { subtree: true, childList: true });
    }
    const entry: ObserveEntry = [selector, cb as ObserveCallback<Element>];
    observables.add(entry);
    const unload = () => {
        observables.delete(entry);
        if (observables.size === 0) sharedObserver.disconnect();
    };
    unloads.add(unload);
    return unload;
}

export function observePromise<T extends Element = Element>(
    unloads: Set<() => void>,
    selector: string,
    timeoutMs: number = 1000,
): Promise<T | null> {
    return new Promise<T | null>((resolve) => {
        const existing = document.querySelector(selector);
        if (existing) {
            resolve(existing as T);
            return;
        }
        const unob = observe<T>(unloads, selector, (elem) => {
            unob();
            clearTimeout(timeout);
            resolve(elem as T);
        });
        const timeout = setTimeout(() => {
            unob();
            resolve(null);
        }, timeoutMs);
    });
}
