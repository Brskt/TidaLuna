export const sendIpc = (channel: string, ...args: any[]) => {
    window.ipc.postMessage(JSON.stringify({ channel, args }));
};
