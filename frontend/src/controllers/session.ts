import { sendIpc } from "../ipc";

interface SessionDelegate {
    onSessionChanged?: (session: any) => void;
}

export const createUserSession = () => {
    let delegate: SessionDelegate | null = (window as any).__LUNAR_SESSION_DELEGATE__ || null;
    let currentSession: any = null;

    // If clear happened before the bundle loaded, consume the flag now.
    if ((window as any).__LUNAR_SESSION_CLEAR_DONE__) {
        currentSession = null;
        (window as any).__LUNAR_SESSION_CLEAR_DONE__ = false;
    }

    return {
        clear: () => {
            sendIpc("jsrt.session_clear");
        },
        registerDelegate: (d: SessionDelegate) => {
            delegate = d;
            (window as any).__LUNAR_SESSION_DELEGATE__ = d;
        },
        update: (session: any) => {
            currentSession = session;
            delegate?.onSessionChanged?.(session);
        },
    };
};
