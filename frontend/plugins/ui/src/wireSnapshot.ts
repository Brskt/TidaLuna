import { ipcRenderer } from "@luna/lib";
import type { LunaUnloads } from "@luna/core";
import type { Store } from "./store";

/**
 * Attach a store to a backend snapshot: the pushes on `channel`, and one pull on the same name for
 * any push sent before this listened. Wired once until the unloads run; a refused pull leaves the
 * store as it was.
 */
export function wireSnapshot<S>(store: Store<S>, channel: string, accept: (state: S, raw: unknown) => S): (unloads: LunaUnloads) => void {
	let wired = false;
	return (unloads) => {
		if (wired) return;
		wired = true;
		ipcRenderer.on(unloads, channel, (snapshot: unknown) => store.publish(accept(store.get(), snapshot)));
		unloads.add(() => {
			wired = false;
		});
		ipcRenderer
			.invoke(channel)
			.then((snapshot: unknown) => store.publish(accept(store.get(), snapshot)))
			.catch(() => {});
	};
}
