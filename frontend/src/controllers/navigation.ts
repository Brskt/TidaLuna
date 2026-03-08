export const createNavigationController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => { delegate = d; },
        goBack: () => { if (delegate && delegate.goBack) delegate.goBack(); },
        goForward: () => { if (delegate && delegate.goForward) delegate.goForward(); },

    }
}

// Navigate the SPA to a given path by pushing history state and
// dispatching a popstate event (React Router listens for these).
export const navigateTo = (path: string) => {
    window.history.pushState({}, "", path);
    window.dispatchEvent(new PopStateEvent("popstate"));
};
(window as any).__TL_NAVIGATE__ = navigateTo;
