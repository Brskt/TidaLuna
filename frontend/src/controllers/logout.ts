// The native menu's Log Out. TIDAL signs out through its own `session/LOGOUT` saga, which posts
// `auth.tidal.com/v1/web-session/logout`; navigating to `/logout` only reaches an AccessDenied page.

type Dispatch = (action: { type: string; payload?: unknown }) => unknown;

// The payload is required, whatever TIDAL's own type for it says: without `initiatedByUser`, or
// with `keepWebSession`, the saga signs the page out without posting the logout.
export const requestLogout = (dispatch: Dispatch): unknown =>
    dispatch({ type: "session/LOGOUT", payload: { initiatedByUser: true } });

// Pinned rather than assigned, since a plugin can replace a plain slot. The dispatch is the store's
// raw one, captured before any plugin loads: an interceptor can cancel the patched dispatch.
export const pinLogout = (target: object, dispatch: Dispatch): void => {
    Object.defineProperty(target, "__TL_LOGOUT__", {
        value: () => requestLogout(dispatch),
        writable: false,
        configurable: false,
        enumerable: false,
    });
};
