export const createNavigationController = () => {
    let delegate: any = null;
    return {
        registerDelegate: (d: any) => { delegate = d; },
        goBack: () => { if (delegate && delegate.goBack) delegate.goBack(); },
        goForward: () => { if (delegate && delegate.goForward) delegate.goForward(); },

    }
}
