export type ReduxInterceptor = (action: any) => any | false | void;

export interface ReduxState {
    store: any;
    actions: Record<string, (...args: any[]) => any>;
    intercept: {
        (actionType: string | string[], unloads: Set<() => void>, interceptor: ReduxInterceptor): void;
        (actionType: string, interceptor: ReduxInterceptor): () => void;
    };
}

const interceptors = new Map<string, Set<ReduxInterceptor>>();

// --- Redux store discovery via React Fiber ---

function isReduxStore(obj: any): boolean {
    return (
        obj != null &&
        typeof obj === "object" &&
        typeof obj.getState === "function" &&
        typeof obj.dispatch === "function" &&
        typeof obj.subscribe === "function"
    );
}

function getFiberKey(el: Element): string | undefined {
    return Object.keys(el).find(
        (k) => k.startsWith("__reactFiber$") || k.startsWith("__reactInternalInstance$"),
    );
}

function findReactRoot(): { element: Element; fiberKey: string } | null {
    const candidates = [
        document.getElementById("wimp"),
        document.getElementById("root"),
        document.getElementById("__next"),
        document.querySelector("[data-reactroot]"),
    ];

    for (const el of candidates) {
        if (!el) continue;
        const key = getFiberKey(el);
        if (key) return { element: el, fiberKey: key };
        for (const child of el.children) {
            const childKey = getFiberKey(child);
            if (childKey) return { element: child, fiberKey: childKey };
        }
    }

    for (const child of document.body.children) {
        const key = getFiberKey(child);
        if (key) return { element: child, fiberKey: key };
        for (const grandchild of child.children) {
            const gKey = getFiberKey(grandchild);
            if (gKey) return { element: grandchild, fiberKey: gKey };
        }
    }

    return null;
}

function findStoreViaReactFiber(): any | null {
    const reactRoot = findReactRoot();
    if (!reactRoot) return null;

    const queue: any[] = [(reactRoot.element as any)[reactRoot.fiberKey]];
    const seen = new WeakSet();
    while (queue.length > 0) {
        const fiber = queue.shift();
        if (!fiber || seen.has(fiber)) continue;
        seen.add(fiber);

        if (isReduxStore(fiber.memoizedProps?.store)) {
            return fiber.memoizedProps.store;
        }
        if (isReduxStore(fiber.memoizedProps?.value?.store)) {
            return fiber.memoizedProps.value.store;
        }

        if (fiber.child) queue.push(fiber.child);
        if (fiber.sibling) queue.push(fiber.sibling);
    }
    return null;
}

export function findReduxStore(): any | null {
    return findStoreViaReactFiber();
}

// --- Patch dispatch + build ReduxState ---

export function patchAndBuildRedux(store: any): ReduxState {
    const originalDispatch = store.dispatch.bind(store);
    store.dispatch = (action: any) => {
        if (action && action.type) {
            const hooks = interceptors.get(action.type);
            if (hooks) {
                // Pass payload to interceptors (RTK convention),
                // fall back to full action for plain actions
                const hookArg =
                    action.payload !== undefined ? action.payload : action;
                for (const hook of hooks) {
                    try {
                        const result = hook(hookArg);
                        if (result === false) return;
                    } catch (e) {
                        console.error(
                            `[luna:redux] Interceptor error for ${action.type}:`,
                            e,
                        );
                    }
                }
            }
        }
        return originalDispatch(action);
    };

    const actions = new Proxy(
        {} as Record<string, (...args: any[]) => any>,
        {
            get(_target, prop: string) {
                return (...args: any[]) => {
                    return store.dispatch({ type: prop, payload: args[0] });
                };
            },
        },
    );

    return {
        store,
        actions,
        intercept: (
            actionType: string | string[],
            unloadsOrInterceptor: Set<() => void> | ReduxInterceptor,
            maybeInterceptor?: ReduxInterceptor,
        ): any => {
            // 3-arg: (type | type[], unloads, callback)
            // 2-arg: (type, callback) => cleanup
            const isThreeArg = maybeInterceptor !== undefined;
            const interceptor = isThreeArg
                ? maybeInterceptor
                : (unloadsOrInterceptor as ReduxInterceptor);
            const types = Array.isArray(actionType) ? actionType : [actionType];

            const cleanups: (() => void)[] = [];
            for (const type of types) {
                let hooks = interceptors.get(type);
                if (!hooks) {
                    hooks = new Set();
                    interceptors.set(type, hooks);
                }
                hooks.add(interceptor);
                cleanups.push(() => hooks!.delete(interceptor));
            }

            const cleanup = () => cleanups.forEach((fn) => fn());

            if (isThreeArg) {
                (unloadsOrInterceptor as Set<() => void>).add(cleanup);
            }
            return cleanup;
        },
    };
}

export function clearInterceptors() {
    interceptors.clear();
}
