export const createUserSession = () => {
    let delegate: any = null;
    return {
        clear: () => { },
        registerDelegate: (d: any) => { delegate = d; },
        update: (s: any) => { },
    }
}
