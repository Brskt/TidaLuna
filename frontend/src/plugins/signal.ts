/**
 * Minimal reactive Signal - drop-in for @inrixia/helpers Signal.
 * Holds a value, notifies listeners on change.
 *
 * Access pattern: signal._ (get/set), signal.onValue(cb) → unsubscribe
 */
export class Signal<T> {
    private _value: T;
    private listeners = new Set<(value: T) => void>();

    constructor(initial: T) {
        this._value = initial;
    }

    get _(): T {
        return this._value;
    }

    set _(value: T) {
        if (this._value === value) return;
        this._value = value;
        for (const listener of this.listeners) {
            try {
                listener(value);
            } catch (e) {
                console.error(e);
            }
        }
    }

    onValue(callback: (value: T) => void): () => void {
        this.listeners.add(callback);
        return () => {
            this.listeners.delete(callback);
        };
    }
}
