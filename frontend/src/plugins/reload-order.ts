export type OrderNode = {
    url: string;
    name: string;
    // Declared dependency names: Rust matches dependants by name, the registry keys by url.
    dependencies: readonly string[];
};

// Splits the plugins into groups tied by declared dependencies, each listed dependants first,
// the order Rust's disable guard accepts; reversed, it is the order its enable guard accepts.
// Members of a cycle come last in input order, and Rust refuses whatever it must.
export function leafFirstGroups(nodes: readonly OrderNode[]): string[][] {
    const dependenciesOf = new Map<string, Set<string>>();
    for (const node of nodes) {
        const targets = new Set<string>();
        for (const name of node.dependencies) {
            for (const other of nodes) {
                if (other !== node && other.name === name) targets.add(other.url);
            }
        }
        dependenciesOf.set(node.url, targets);
    }

    const root = new Map(nodes.map((node) => [node.url, node.url]));
    const find = (url: string): string => {
        let at = url;
        while (root.get(at) !== at) at = root.get(at)!;
        root.set(url, at);
        return at;
    };
    for (const [url, targets] of dependenciesOf) {
        for (const target of targets) root.set(find(url), find(target));
    }

    const groups = new Map<string, string[]>();
    for (const node of nodes) {
        const key = find(node.url);
        const group = groups.get(key);
        if (group) group.push(node.url);
        else groups.set(key, [node.url]);
    }

    return [...groups.values()].map((members) => {
        const left = new Set(members);
        const ordered: string[] = [];
        while (left.size > 0) {
            const free = [...left].filter((url) => ![...left].some((other) => dependenciesOf.get(other)!.has(url)));
            const taken = free.length > 0 ? free : [...left];
            for (const url of taken) {
                ordered.push(url);
                left.delete(url);
            }
        }
        return ordered;
    });
}
