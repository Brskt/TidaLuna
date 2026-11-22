import { sendIpc, isLoginCallback } from "../ipc";

interface SessionDelegate {
    onSessionChanged?: (session: any) => void;
}

export const createUserSession = () => {
    let delegate: SessionDelegate | null = (window as any).__LUNAR_SESSION_DELEGATE__ || null;
    let currentSession: any = null;

    return {
        clear: () => {
            if (isLoginCallback()) return;
            sendIpc("jsrt.session_clear", (window as any).__LUNAR_PAGE__);
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
