// The controller's shuffle and repeat, shown by this page while it plays a TIDAL Connect cast.
//
// TIDAL's player bar reads `playQueue.repeatMode` (0 off, 1 all, 2 one) and
// `playQueue.shuffleModeEnabled`, and saves both as the user's own player settings whenever they
// change. The page's own modes are kept when a cast first shows the controller's, and given back
// when the cast ends (a cast leaves the user's settings as it found them).

import { isStoreNotPublished } from "../../plugins/lib/src/redux/storeNotPublished";
import { storePublished } from "../store-published";
import type { ReceiverModesPayload, RepeatMode } from "./types";

type Store = {
    dispatch: (action: { type: string; payload?: unknown }) => void;
    getState: () => any;
};

type PageModes = { repeatMode: number; shuffled: boolean };

// Kept across a reload of the page mid-cast, which would otherwise read the controller's modes
// back from TIDAL's saved settings as the user's own.
const KEPT_KEY = "luna.connect.receiverPageModes";

let shown: PageModes | undefined;

const REPEAT_ENUM: Record<RepeatMode, number> = { OFF: 0, ALL: 1, ONE: 2 };

function readPage(store: Store): PageModes {
    const queue = store.getState()?.playQueue;
    return { repeatMode: queue?.repeatMode ?? 0, shuffled: queue?.shuffleModeEnabled === true };
}

function readKept(): PageModes | undefined {
    try {
        const raw = sessionStorage.getItem(KEPT_KEY);
        return raw ? (JSON.parse(raw) as PageModes) : undefined;
    } catch {
        return undefined;
    }
}

function writeKept(modes: PageModes | undefined): void {
    try {
        if (modes) sessionStorage.setItem(KEPT_KEY, JSON.stringify(modes));
        else sessionStorage.removeItem(KEPT_KEY);
    } catch {
        // Storage refused: the page's modes are only lost on a reload mid-cast.
    }
}

// Each write only when it changes the store: TIDAL saves the settings on every one.
function apply(store: Store, modes: PageModes): void {
    const page = readPage(store);
    if (page.repeatMode !== modes.repeatMode) {
        store.dispatch({ type: "playQueue/SET_REPEAT_MODE", payload: modes.repeatMode });
    }
    if (page.shuffled !== modes.shuffled) {
        store.dispatch({ type: modes.shuffled ? "playQueue/ENABLE_SHUFFLE_MODE" : "playQueue/DISABLE_SHUFFLE_MODE" });
    }
}

/**
 * Show the controller's modes, keeping the page's own first if no cast kept them yet. A cast
 * during boot lands before TIDAL's store: the modes wait for it, unless newer ones came meanwhile.
 */
export async function showReceiverModes(
    store: Store,
    payload: ReceiverModesPayload,
    published: () => Promise<void> = storePublished,
): Promise<void> {
    const modes = { repeatMode: REPEAT_ENUM[payload.repeatMode] ?? 0, shuffled: payload.shuffled === true };
    shown = modes;
    try {
        store.getState();
    } catch (e) {
        if (!isStoreNotPublished(e)) throw e;
        await published();
        if (shown !== modes) return;
    }
    if (!readKept()) writeKept(readPage(store));
    apply(store, modes);
}

/** Whether a cast's modes are on the player bar: its shuffle and repeat buttons are the cast's. */
export function castShowsModes(): boolean {
    return shown !== undefined;
}

/** The cast reset the page's queue (`playQueue/RESET`): its modes are shown again. */
export function reshowReceiverModes(store: Store): void {
    if (shown) apply(store, shown);
}

/** The cast is over: the page gets back the modes it had before it. */
export function releaseReceiverModes(store: Store): void {
    shown = undefined;
    const kept = readKept();
    if (!kept) return;
    writeKept(undefined);
    apply(store, kept);
}
