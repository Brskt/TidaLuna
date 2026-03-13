/**
 * ContextMenu — port of TidaLuna's @luna/lib ContextMenu class.
 *
 * Allows plugins to add buttons to TIDAL's context menus.
 * Intercepts contextMenu/OPEN Redux action to detect when menus open.
 *
 * Usage (from plugins):
 *   const button = ContextMenu.addButton(unloads);
 *   button.text = "My Action";
 *   button.onClick((e) => { ... });
 *   ContextMenu.onOpen(unloads, ({ contextMenu }) => button.show(contextMenu));
 */

// --- ContextMenuButton ---

class ContextMenuButton {
    private _element?: HTMLSpanElement;
    private _text = "";
    private _onClick?: (ev: MouseEvent) => any;

    get elem(): HTMLSpanElement | undefined {
        return this._element;
    }

    set elem(elem: HTMLSpanElement | undefined) {
        if (this._element) this._element.remove();
        this._element = elem;
        if (!this._element) return;
        this._element.innerText = this._text;
        this._element.onclick = (e) => {
            e.preventDefault();
            this._onClick?.(e);
        };
    }

    get text(): string {
        return this._text;
    }

    set text(value: string) {
        this._text = value;
        if (this._element) this._element.innerText = value;
    }

    onClick(cb: (ev: MouseEvent) => any) {
        this._onClick = cb;
        if (this._element) {
            this._element.onclick = (e) => {
                e.preventDefault();
                this._onClick?.(e);
            };
        }
    }

    async show(contextMenu: Element | null): Promise<HTMLSpanElement | undefined> {
        if (!contextMenu) return this._element;

        const templateButton = contextMenu.querySelector(
            `div[data-type="contextmenu-item"]`,
        ) as Element | undefined;

        if (!this._element && templateButton) {
            const newButton = templateButton.cloneNode(true) as Element;
            const btn = newButton.querySelector<HTMLButtonElement>("button");
            if (btn) btn.removeAttribute("data-test");
            const span = newButton.querySelector<HTMLSpanElement>("span");
            if (span) this.elem = span;
        }

        if (this._element?.parentElement?.parentElement) {
            contextMenu.appendChild(this._element.parentElement.parentElement);
        }

        return this._element;
    }
}

// --- ContextMenu ---

type OnOpenData = { event?: any; contextMenu: Element };
type OpenListener = {
    cb: (data: OnOpenData) => void;
};

export class ContextMenu {
    private static buttons = new Set<ContextMenuButton>();
    private static openListeners: OpenListener[] = [];

    /**
     * Register a button to be shown in context menus.
     * Returns a ContextMenuButton with .text, .onClick(), .show() API.
     */
    static addButton(unloads: Set<() => void>): ContextMenuButton {
        const button = new ContextMenuButton();
        ContextMenu.buttons.add(button);
        unloads.add(() => {
            ContextMenu.buttons.delete(button);
            button.elem?.remove();
        });
        return button;
    }

    /**
     * Find the currently open context menu in the DOM (1s timeout).
     */
    static async getCurrent(): Promise<Element | null> {
        const selector = '[data-type="list-container__context-menu"]';
        const existing = document.querySelector(selector);
        if (existing) return existing;

        return new Promise<Element | null>((resolve) => {
            const observer = new MutationObserver(() => {
                const el = document.querySelector(selector);
                if (el) {
                    observer.disconnect();
                    clearTimeout(timer);
                    resolve(el);
                }
            });
            observer.observe(document.body, { childList: true, subtree: true });
            const timer = setTimeout(() => {
                observer.disconnect();
                resolve(null);
            }, 1000);
        });
    }

    /**
     * Register a listener that fires when a context menu opens.
     * Signature matches TidaLuna's AddReceiver pattern.
     */
    static onOpen(
        unloads: Set<() => void>,
        callback: (data: OnOpenData) => void,
    ) {
        const entry: OpenListener = { cb: callback };
        ContextMenu.openListeners.push(entry);
        unloads.add(() => {
            const idx = ContextMenu.openListeners.indexOf(entry);
            if (idx !== -1) ContextMenu.openListeners.splice(idx, 1);
        });
    }

    /**
     * Wire up Redux interception for contextMenu/OPEN.
     * Must be called after Redux is discovered and patched.
     */
    static setupInterceptors(
        redux: { intercept: Function },
        unloads: Set<() => void>,
    ) {
        redux.intercept("contextMenu/OPEN", unloads, async (event: any) => {
            const contextMenu = await ContextMenu.getCurrent();
            if (contextMenu === null) return;

            // Show all registered buttons
            for (const button of ContextMenu.buttons) {
                button.show(contextMenu);
            }

            // Fire onOpen listeners
            for (const listener of ContextMenu.openListeners) {
                try {
                    listener.cb({ event, contextMenu });
                } catch (e) {
                    console.error("[luna:contextmenu] onOpen listener error:", e);
                }
            }
        });
    }
}
