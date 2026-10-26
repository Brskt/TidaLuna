import { showErrorBanner } from "./banner";

/** What Rust repaired in settings.db at launch (`RepairNotice` in src/settings.rs). */
export type StoreRepair = { turnedOff: string[]; trust: "empty" | "dropped" | "unknown"; saved: boolean };

// `empty` says nothing: no permission was lost.
const TRUST_SENTENCES = {
    empty: null,
    dropped: "Native plugins will ask for permission again.",
    unknown: "Native plugins may ask for permission again.",
} as const;

// Plain names for the settings a loss writes down as off.
const SETTING_NAMES: Record<string, string> = {
    "connect.receiver_always_on": "Connect",
    "video.codec_enabled": "the video codec",
    "plugins.update_at_boot": "the plugin update check",
};

const capitalized = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);

export const storeRepairMessage = ({ turnedOff, trust, saved }: StoreRepair): string => {
    const parts = [
        saved
            ? "TidaLunar repaired its damaged settings."
            : "TidaLunar could not repair its settings file. Changes will not be saved until the next launch.",
    ];
    if (turnedOff.length > 0) {
        const names = turnedOff.map((key) => SETTING_NAMES[key] ?? key);
        const listed = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}` : names[0];
        parts.push(`${capitalized(listed)} ${names.length > 1 ? "were" : "was"} turned off to be safe.`);
    }
    const trustSentence = TRUST_SENTENCES[trust];
    if (trustSentence) parts.push(trustSentence);
    return parts.join(" ");
};

/** What Rust did to plugins.db at launch (`PluginsReset` in src/plugins/store.rs): the copies of the damaged file. */
export type PluginsReset = { asides: string[] };

/** What the page owes the user of the repairs at launch: the settings fields are there only when settings.db was repaired. */
export type PendingRepairs = Partial<StoreRepair> & { plugins?: PluginsReset };

const isSettingsRepair = (pending: PendingRepairs): pending is StoreRepair =>
    pending.turnedOff !== undefined && pending.trust !== undefined && pending.saved !== undefined;

export const pluginsResetMessage = ({ asides }: PluginsReset): string => {
    const parts = ["TidaLunar replaced its damaged plugin database with an empty one. Your installed plugins and their stored data are gone."];
    if (asides.length === 1) parts.push(`A copy of the damaged file is kept as ${asides[0]}.`);
    if (asides.length > 1) parts.push(`Copies of the damaged files are kept as ${asides.join(", ")}.`);
    return parts.join(" ");
};

export const pendingRepairsMessage = (pending: PendingRepairs): string => {
    const parts: string[] = [];
    if (isSettingsRepair(pending)) parts.push(storeRepairMessage(pending));
    if (pending.plugins) parts.push(pluginsResetMessage(pending.plugins));
    return parts.join(" ");
};

/** What the page can see of the screen: the banners now in the DOM, and a call on every change to it. */
export type BannerScreen = {
    shown: (message: string) => boolean;
    watch: (onChange: () => void) => () => void;
};

/**
 * TIDAL renders its notification list inside the main layout, and renders a loading overlay in
 * its place while the route is `/login`: a banner dispatched there sits in the store, unseen, and a
 * reload drops it. The DOM is the only place that says the list is up; that is what is read.
 */
export const bannerScreen = (
    doc: Pick<Document, "querySelectorAll" | "body">,
    observe: (onChange: () => void, target: Node) => () => void,
): BannerScreen => ({
    shown: (message) =>
        Array.from(doc.querySelectorAll('[data-test="notification"]')).some((el) => el.textContent?.includes(message)),
    watch: (onChange) => observe(onChange, doc.body),
});

export const observeMutations = (onChange: () => void, target: Node): (() => void) => {
    const observer = new MutationObserver(onChange);
    observer.observe(target, { childList: true, subtree: true, characterData: true });
    return () => observer.disconnect();
};

/**
 * Asks Rust for the repair the listener has not been told of, banners it, and acknowledges only
 * once the banner is on the screen. An ask that fails or never answers, a banner that throws, or a
 * page that ends before the banner shows (the login route, then a reload) leaves the notice owed
 * for the next page. Resolves when acknowledged; never resolves for a page that ends first.
 */
export const deliverStoreRepair = async (
    store: { dispatch: (action: unknown) => void },
    ask: () => Promise<PendingRepairs | null>,
    screen: BannerScreen,
    acknowledge: () => void,
): Promise<void> => {
    const repair = await ask();
    if (!repair) return;
    const message = pendingRepairsMessage(repair);
    showErrorBanner(store, "OTHER", message);
    if (screen.shown(message)) return acknowledge();
    await new Promise<void>((resolve) => {
        let seen = false;
        let stop: (() => void) | undefined;
        stop = screen.watch(() => {
            if (seen || !screen.shown(message)) return;
            seen = true;
            stop?.();
            resolve();
        });
        if (seen) stop();
    });
    acknowledge();
};
