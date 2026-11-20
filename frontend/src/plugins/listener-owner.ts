// A plugin's listener runs under one error handler shared by every listener of its registrar,
// which logged under @luna/lib. The plugin's own copy of the lib wraps each listener it registers,
// so a throw lands on the card of the plugin that registered it.

type AnyFn = (...args: any[]) => unknown;

/** Takes a listener's error; must never throw, registerEmitter calls its handler unguarded. */
export type ReportListenerError = (registrar: string, err: unknown) => void;

// Taken before any plugin runs: a plugin patching these must not reach the reporting path.
const apply = Reflect.apply;
const then = Promise.prototype.then;

// The lib's registrars that take a plugin's listener, and the argument it comes in.
const STATIC_REGISTRARS = {
	MediaItem: ["onPreload", "onMediaTransition", "onPreMediaTransition"],
	ContextMenu: ["onOpen", "onMediaItem"],
	PlayState: ["onScrobble", "onState"],
} as const;
const RECEIVER_ARG = 1;
const INTERCEPT_ARG = 2;
const IPC_LISTENER_ARG = 2;
const OPEN_URL_LISTENER_ARG = 1;

/**
 * The listener as its registrar receives it. A throw or a rejection reaches the owner and goes no
 * further, as the shared handler's log did: the next listener runs and an intercept is not cancelled.
 */
export function own(report: ReportListenerError, registrar: string, listener: AnyFn): AnyFn {
	return function (this: unknown, ...args: unknown[]) {
		let result: unknown;
		try {
			result = apply(listener, this, args);
		} catch (err) {
			report(registrar, err);
			return undefined;
		}
		if (!(result instanceof Promise)) return result;
		try {
			return apply(then, result, [
				undefined,
				(err: unknown) => {
					report(registrar, err);
					return undefined;
				},
			]);
		} catch (err) {
			report(registrar, err);
			return undefined;
		}
	};
}

// One wrapper per listener and registrar: registries are Sets, and a listener added twice runs once.
function wrapperCache(report: ReportListenerError) {
	const byRegistrar = new Map<string, WeakMap<AnyFn, AnyFn>>();
	return (registrar: string) => (listener: AnyFn) => {
		let byListener = byRegistrar.get(registrar);
		if (byListener === undefined) byRegistrar.set(registrar, (byListener = new WeakMap()));
		let wrapped = byListener.get(listener);
		if (wrapped === undefined) byListener.set(listener, (wrapped = own(report, registrar, listener)));
		return wrapped;
	};
}

function ownedRegistrar(registrar: AnyFn, listenerAt: number, wrap: (listener: AnyFn) => AnyFn): AnyFn {
	return function (this: unknown, ...args: unknown[]) {
		if (typeof args[listenerAt] === "function") args[listenerAt] = wrap(args[listenerAt] as AnyFn);
		return apply(registrar, this, args);
	};
}

// The class itself for everything else: statics written through `this` reach the shared state.
function facade<C extends object>(cls: C, wrapped: Record<string, AnyFn>): C {
	return new Proxy(cls, {
		get: (target, key) => (typeof key === "string" && Object.hasOwn(wrapped, key) ? wrapped[key] : Reflect.get(target, key, target)),
	});
}

/** A copy of the lib whose registrars wrap each listener with `report`; everything else is the lib's. */
export function withOwnedListeners<L extends Record<string, any>>(lib: L, report: ReportListenerError): L {
	const wrapperFor = wrapperCache(report);
	const copy: Record<string, unknown> = { ...lib };
	for (const [className, statics] of Object.entries(STATIC_REGISTRARS)) {
		const cls = lib[className];
		const wrapped: Record<string, AnyFn> = {};
		for (const name of statics) {
			const registrar = `${className}.${name}`;
			wrapped[name] = ownedRegistrar(cls[name], RECEIVER_ARG, wrapperFor(registrar));
		}
		copy[className] = facade(cls, wrapped);
	}
	copy.redux = { ...lib.redux, intercept: ownedRegistrar(lib.redux.intercept, INTERCEPT_ARG, wrapperFor("redux.intercept")) };
	copy.ipcRenderer = {
		...lib.ipcRenderer,
		on: ownedRegistrar(lib.ipcRenderer.on, IPC_LISTENER_ARG, wrapperFor("ipcRenderer.on")),
		once: ownedRegistrar(lib.ipcRenderer.once, IPC_LISTENER_ARG, wrapperFor("ipcRenderer.once")),
		onOpenUrl: ownedRegistrar(lib.ipcRenderer.onOpenUrl, OPEN_URL_LISTENER_ARG, wrapperFor("ipcRenderer.onOpenUrl")),
	};
	return copy as L;
}

/** An error as one line for a card: never throws, whatever the plugin threw. */
export function describeError(err: unknown): string {
	const MAX = 200;
	try {
		const text = err instanceof Error ? String(err.message) : String(err);
		return text.length > MAX ? `${text.slice(0, MAX)}...` : text;
	} catch {
		return "an error that cannot be read";
	}
}
