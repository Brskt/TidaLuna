// Memoize Intl.DateTimeFormat.
//
// TIDAL builds a fresh Intl.DateTimeFormat on every time/date render tick
// instead of reusing one. Each construction allocates a JS object, a locale
// string, three ICU C++ Managed handles and a closure, churning RAM and CPU.
// A formatter is immutable for a given (locales, options): format(),
// formatToParts() and resolvedOptions() are pure, and a shared cached instance
// behaves identically. TIDAL uses only a handful of distinct (locales, options)
// combinations; the cache stays tiny.

// Cacheable only when the value serializes to a faithful key. Intl.Locale
// objects and other non-string locales are excluded (they stringify to {}).
function isSafeLocales(v: unknown): boolean {
    if (v === undefined) return true;
    if (typeof v === "string") return true;
    if (Array.isArray(v)) return v.every((s) => typeof s === "string");
    return false;
}

// Plain object of primitive values only; null is excluded for the native
// constructor to throw on it as the spec requires.
function isSafeOptions(v: unknown): boolean {
    if (v === undefined) return true;
    if (v === null || typeof v !== "object" || Array.isArray(v)) return false;
    if (Object.getPrototypeOf(v) !== Object.prototype) return false;
    return Object.values(v as Record<string, unknown>).every(
        (val) =>
            val === undefined ||
            val === null ||
            typeof val === "string" ||
            typeof val === "boolean" ||
            typeof val === "number",
    );
}

export function installDateTimeFormatMemo(): void {
    const Original = Intl.DateTimeFormat;
    if ((Original as { __memoized?: boolean }).__memoized) return;

    const cache = new Map<string, Intl.DateTimeFormat>();
    const MAX_ENTRIES = 256;

    const Memoized = function (locales?: unknown, options?: unknown) {
        // Subclassing (class X extends Intl.DateTimeFormat) must receive a
        // fresh, correctly-prototyped instance, never a shared cached one.
        const target = new.target as unknown;
        if (target !== undefined && target !== Memoized) {
            return Reflect.construct(Original, [locales, options], target as new (...a: unknown[]) => object);
        }

        // Only cache when the args serialize faithfully to the key. Bypass for
        // anything JSON.stringify would lose or conflate: notably Intl.Locale
        // objects (stringify to {} and collide) and null (which the native
        // constructor must be allowed to throw on). Semantics are preserved.
        if (!isSafeLocales(locales) || !isSafeOptions(options)) {
            return new (Original as any)(locales, options);
        }

        let key: string;
        try {
            key = JSON.stringify([locales ?? null, options ?? null]);
        } catch {
            // Non-serialisable options should never occur for date formatting;
            // bypass the cache rather than throw.
            return new (Original as any)(locales, options);
        }

        let instance = cache.get(key);
        if (instance === undefined) {
            instance = new (Original as any)(locales, options);
            // The instance is shared across all callers of this key: freeze it
            // to prevent one caller mutating it (e.g. `fmt.format = ...`) from
            // poisoning the others. Prototype methods (format/resolvedOptions) are
            // unaffected by freezing the instance.
            Object.freeze(instance);
            if (cache.size >= MAX_ENTRIES) {
                cache.delete(cache.keys().next().value as string);
            }
            cache.set(key, instance!);
        }
        return instance;
    };

    // instanceof checks and prototype-method lookups must resolve against the
    // original prototype; the static supportedLocalesOf must stay available.
    const memo = Memoized as unknown as {
        prototype: unknown;
        supportedLocalesOf: unknown;
        __memoized: boolean;
    };
    memo.prototype = Original.prototype;
    memo.supportedLocalesOf = Original.supportedLocalesOf.bind(Original);
    memo.__memoized = true;
    Object.defineProperty(Memoized, "name", { value: "DateTimeFormat", configurable: true });

    Intl.DateTimeFormat = Memoized as unknown as typeof Intl.DateTimeFormat;
}
