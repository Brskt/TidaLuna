export const createUserSettings = () => {
    let settings = new Map<string, any>();
    let logCount = 0;
    const LOG_CAP = 20;
    const log = (tag: string, key: string, value?: any) => {
        if (logCount >= LOG_CAP) {
            if (logCount === LOG_CAP) console.warn(`[Settings] Log cap (${LOG_CAP}) reached, silencing`);
            logCount++;
            return;
        }
        logCount++;
        console.log(`[Settings] ${tag}`, key, value !== undefined ? value : "");
    };
    return {
        get: (key: string) => { log("get", key); return settings.get(key); },
        set: (key: string, value: any) => { log("set", key, value); settings.set(key, value); },
    }
}
