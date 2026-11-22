// The native menu's Log Out: TIDAL's own sign-out, dispatched on the store's raw dispatch through
// a global no plugin can replace or redefine.

import { expect, test } from "bun:test";
import { pinLogout, requestLogout } from "../src/controllers/logout";

type Action = { type: string; payload?: unknown };

test("the sign-out names the user as its initiator, the only shape TIDAL's saga posts for", () => {
    const sent: Action[] = [];
    requestLogout((a) => sent.push(a));
    expect(sent).toEqual([{ type: "session/LOGOUT", payload: { initiatedByUser: true } }]);
});

test("the pinned Log Out reaches the dispatch it was pinned with, and a plugin can neither replace nor hide it", () => {
    const sent: Action[] = [];
    const target: Record<string, unknown> = {};
    pinLogout(target, (a) => sent.push(a));

    (target.__TL_LOGOUT__ as () => void)();
    expect(sent).toHaveLength(1);

    const pinned = target.__TL_LOGOUT__;
    expect(() => {
        target.__TL_LOGOUT__ = () => {};
    }).toThrow(TypeError);
    expect(() => Object.defineProperty(target, "__TL_LOGOUT__", { value: () => {} })).toThrow(TypeError);
    expect(() => delete target.__TL_LOGOUT__).toThrow(TypeError);
    expect(target.__TL_LOGOUT__).toBe(pinned);
    expect(Object.keys(target)).not.toContain("__TL_LOGOUT__");
});
