export const createUserSettings = () => {
    let settings = new Map<string, any>();
    return {
        get: (key: string) => settings.get(key),
        set: (key: string, value: any) => { settings.set(key, value); },
    }
}
