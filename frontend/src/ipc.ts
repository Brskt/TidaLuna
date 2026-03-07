export const sendIpc = (channel: string, ...args: any[]) => {
    window.cefQuery({
        request: JSON.stringify({ channel, args }),
        onSuccess: () => {},
        onFailure: (code: number, msg: string) => console.error("[IPC] FAIL:", channel, code, msg),
    });
};
