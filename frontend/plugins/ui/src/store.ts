export type Store<S> = {
	get(): S;
	publish(next: S): void;
	subscribe(listener: (state: S) => void): () => void;
};

/**
 * A value many surfaces read and the plugin's entry feeds. Kept apart from React and from the IPC
 * bridge, for the hooks and the wiring to sit on top of one shell instead of each copying it.
 */
export function createStore<S>(initial: S): Store<S> {
	let current = initial;
	const listeners = new Set<(state: S) => void>();
	return {
		get: () => current,
		publish(next) {
			if (next === current) return;
			current = next;
			for (const listener of listeners) listener(current);
		},
		subscribe(listener) {
			listeners.add(listener);
			return () => {
				listeners.delete(listener);
			};
		},
	};
}
