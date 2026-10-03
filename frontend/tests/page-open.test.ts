// `Page.open()` against a stand-in for TIDAL's router: a `router/PUSH` saga that hands its
// payload to `router.navigate(to)`, which commits with `history.push`.

import { beforeEach, expect, mock, test } from "bun:test";

import type { RouterTo } from "../plugins/lib/src/redux/types/actions";

const pushes: RouterTo[] = [];

const toPath = (to: RouterTo) => {
	if (typeof to === "string") return to;
	const search = to.search && to.search !== "?" ? (to.search.startsWith("?") ? to.search : `?${to.search}`) : "";
	return `${to.pathname ?? "/"}${search}`;
};

mock.module("@luna/lib", () => ({
	redux: {
		intercept: () => {},
		actions: {
			"router/PUSH": (to: RouterTo) => {
				pushes.push(to);
				history.pushState(null, "", toPath(to));
			},
		},
	},
}));

const { Page } = await import("../plugins/ui/src/classes/Page");

// happy-dom starts on about:blank, where `pushState` cannot move the URL
(window as any).happyDOM.setURL("https://desktop.tidal.com/album/1");

beforeEach(() => {
	pushes.length = 0;
	history.replaceState(null, "", "/album/1");
});

test("the first open pushes, so Back returns to the TIDAL page", () => {
	const page = Page.register("settings-first", new Set());
	const before = history.length;

	page.open();

	expect(pushes).toHaveLength(1);
	expect(location.pathname + location.search).toBe("/not-found?settings-first");
	expect(history.length).toBe(before + 1);
});

test("re-opening the page already shown adds no history entry", () => {
	const page = Page.register("settings-again", new Set());
	page.open();
	const after = history.length;

	page.open();
	page.open();

	expect(pushes).toHaveLength(1);
	expect(history.length).toBe(after);
});

test("opening another page from an open one still pushes", () => {
	const a = Page.register("page-a", new Set());
	const b = Page.register("page-b", new Set());
	a.open();

	b.open();

	expect(pushes).toHaveLength(2);
	expect(location.search).toBe("?page-b");
});
